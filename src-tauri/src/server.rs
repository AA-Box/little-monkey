//! Local OpenAI-compatible API server (phases 1-3 of the local-model-hub
//! roadmap item — see `docs/roadmap/p1-local-api-server.md`).
//!
//! This is a *routing reverse proxy*, not a new inference engine: it runs a
//! small hyper-1 HTTP server on a tokio task, bound to `127.0.0.1` only (no
//! LAN bind — that's an explicitly later, separately-gated phase), and
//! [`handle_request`] — the `AppHandle`-free, `monkey-cli`-reusable core —
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
//! Phase 5 (Private Developer API/Embeddable Chat Widget, ROADMAP.md) layers
//! three more routes on top, in [`handle_extended_request`] — reachable only
//! from the GUI's own accept loop (never from `monkey-cli api-serve`, which has
//! no `AppHandle` to give them):
//!
//!   - `POST /v1/knowledge/query`      — [`Scope::Knowledge`], wraps
//!                                       `knowledge_service::knowledge_v2_query`
//!   - `GET  /v1/artifacts/{id}`       — [`Scope::ArtifactRead`], wraps
//!                                       `artifact_commands::artifact_blob_read_base64`
//!   - `GET  /v1/workflows/runs/{id}`  — [`Scope::WorkflowRun`], wraps
//!                                       `run_commands::run_get` (status only)
//!
//! SECURITY: this surface is deliberately narrow. It must NEVER grow a route
//! that reaches the agent's tool-dispatch layer (`tool_run_shell` and
//! friends in `tools.rs`) — doing so would turn a local HTTP server into a
//! remote-code-execution surface for anything that can reach loopback. Any
//! future change to the `match` in [`handle_request`] or
//! [`handle_extended_request`] must preserve that invariant. This is exactly
//! why `Scope::WorkflowRun` only ever gates a *read-only run status* lookup
//! here, never a route that submits a new run: `run_commands::run_submit`
//! only records a `RunSpec` into the durable ledger, but the app's own
//! frontend event loop is what actually turns a ledger entry into live tool
//! execution once it observes one — and that loop has no way to distinguish
//! "a run this same desktop app's user started" from "a run an external HTTP
//! caller just injected". Exposing run-submission here would hand any local
//! process holding a `workflow_run`-scoped token exactly the remote-tool-
//! execution surface this module's core invariant forbids. Wiring an actual
//! "trigger a workflow over the API" route is therefore a deliberate
//! non-goal of this stage, not an oversight — it needs its own dedicated
//! design (most likely an explicit per-run approval, mirroring
//! `permissions.rs`) before it can be added safely.
//!
//! Structured like `checkpoints.rs`/`web.rs`: an `AppHandle`-free,
//! independently testable core ([`handle_request`], [`route_model`]) plus a
//! thin `#[tauri::command]` layer that owns the actual listening socket and
//! `AppState` bookkeeping. `pub` (not `mod`) so a future `monkey-cli` `api-serve`
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
///
/// Drawn from `http_policy` rather than repeating the literal: that module's
/// bind-error message branches on `port == DEFAULT_HTTP_PORT` to name the *other*
/// listener as the likely conflict, so a listener with its own copy of the number
/// could be moved off 1234 and silently make that diagnosis wrong. The shared
/// constant is what makes the collision a stated fact instead of a coincidence.
const DEFAULT_PORT: u16 = crate::http_policy::DEFAULT_HTTP_PORT;

/// Prefix on every generated bearer token, so a leaked token is
/// self-describing (`lmk-<32 hex chars>`) the same way e.g. GitHub's
/// `ghp_`/OpenAI's `sk-` prefixes are.
const TOKEN_PREFIX: &str = "lmk-";

/// Filename for the persisted server config under the app data directory —
/// same file-per-feature pattern as `providers.json`/`web_settings.json`.
const CONFIG_FILE: &str = "api_server.json";

/// Hard cap on a request body this server will buffer into memory, enforced
/// by [`read_capped_body`] as it streams frames in (never after the fact —
/// buffering the whole thing first and checking the length would defeat the
/// point). Chat-completion payloads can legitimately run to several MB
/// (inline base64 image content for multimodal messages), so this is set
/// generously rather than to a tight "small JSON payload" bound — it exists
/// only to put a ceiling on a malicious or mistaken caller's memory impact,
/// the same "streamed cap regardless of what Content-Length claims" stance
/// `web.rs::MAX_BODY_BYTES` takes for fetched page bodies (see the
/// security-review finding this addresses).
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Boxed error type for [`ResponseBody`] — reqwest's streaming errors and
/// our own infallible bodies both erase to this.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Unified response body type: either a fully-buffered JSON payload ([`Full`])
/// or an SSE byte-stream passthrough ([`StreamBody`]), both boxed so
/// [`handle_request`] can return one concrete type regardless of which route
/// it took.
type ResponseBody = BoxBody<Bytes, BoxError>;

/// Keeps an [`AdmissionGuard`] alive until the response body it belongs to is
/// finished or the client stops reading it.
///
/// The bug this fixes: the accept loop dropped its guard as soon as
/// `serve_one_request` *returned a `Response`*, which for the dominant traffic
/// shape is far too early. A streaming `/v1/chat/completions` returns as soon as
/// upstream headers arrive and hands back a [`StreamBody`] wrapping reqwest's
/// `bytes_stream` — not one byte of which has been read yet. So the permit was
/// released before the request did any of its work, and concurrent SSE streams
/// were not bounded by [`http_policy::MAX_ACTIVE_REQUESTS`] at all: the counter
/// measured time-to-first-header, not time in flight.
///
/// Wrapping the body rather than threading the guard into each handler is
/// deliberate. The guard belongs to the accept loop, the body is built deep
/// inside a route, and every route already funnels through one `Response` — so
/// this attaches at the one place both facts are available, and no handler
/// signature has to learn that admission control exists. `m3_http_server.rs`
/// reaches the same end by moving its guard into `sse_body`'s unfold state; it
/// can, because it constructs its own stream and has exactly one streaming shape.
///
/// [`http_body_util::BodyStream`] rather than `into_data_stream` so trailers
/// survive: nothing legacy sends them today, and a wrapper that silently ate
/// them would be a trap for whatever does first.
fn hold_permit_until_body_ends(
    body: ResponseBody,
    guard: crate::http_policy::AdmissionGuard,
) -> ResponseBody {
    // Racing the guard's own token here is safe, and the ordering is the reason:
    // the token is cancelled by `AdmissionGuard::drop`, and the guard lives in
    // this stream's state — so it cannot fire from this stream's own teardown
    // while the stream is still being polled. It fires only from the *parent*
    // token, which the accept loop cancels when it exits. That is a stopping
    // server, and this is where a stream in flight finds out about it.
    let cancel = guard.cancellation();
    // The guard lives in the unfold state, so it drops when the stream finishes
    // *or* when hyper drops the body because the client went away — which is the
    // release this needs, and the reason there is no explicit drop below.
    let stream = futures_util::stream::unfold(
        (http_body_util::BodyStream::new(body), guard, cancel, false),
        |(mut frames, guard, cancel, done)| async move {
            if done {
                return None;
            }
            tokio::select! {
                frame = frames.next() => frame.map(|frame| (frame, (frames, guard, cancel, false))),
                _ = cancel.cancelled() => {
                    // An error, not a clean end. A truncated SSE stream that
                    // closes successfully is indistinguishable to the client from
                    // a completed one that happens to lack `[DONE]` — it would
                    // read a partial answer as the whole answer. `done` ends the
                    // stream on the next poll so the error is emitted exactly
                    // once.
                    let error: BoxError = Box::new(std::io::Error::other(
                        "The API server stopped while this response was streaming",
                    ));
                    Some((Err(error), (frames, guard, cancel, true)))
                }
            }
        },
    );
    BodyExt::boxed(StreamBody::new(stream))
}

/// Awaits `work` unless this request is cancelled first.
///
/// `reqwest` has no cancel method, so cancellation is a race against the request
/// future — dropping the loser is what aborts the connection. `None` means
/// cancelled, and callers owe the client an answer that says so rather than a
/// `502` blaming the upstream for a stop this app initiated.
async fn unless_cancelled<T>(
    cancel: &tokio_util::sync::CancellationToken,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        value = work => Some(value),
        _ = cancel.cancelled() => None,
    }
}

/// The answer a request gets when the server stops mid-flight.
///
/// `503` and not `502`: the upstream did nothing wrong, and a client that retries
/// on `502` would be retrying against a listener that is gone.
fn cancelled_response() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "The API server stopped before this request completed",
        "server_stopping",
    )
}

/// The one implementation of "this listener admits a request".
///
/// Both accept loops call it, which is the point: the legacy listener had two
/// serving paths and only one of them was admitted. `run_cli_server`
/// (`monkey-cli api-serve`) spawned an unbounded task per connection with no
/// permit, no counters and no cancellation, serving the *identical* route set
/// through the same `serve_one_request` — so every legacy route stayed reachable
/// with admission control fully bypassed, while the doc comment above
/// [`run_accept_loop`]'s admission claimed a route could no longer do that.
///
/// `serve` returns the response *and* performs the per-request bookkeeping each
/// loop does differently (token-used records, request logging), so the two loops
/// share the rule without sharing their `ServerDeps` construction. It receives the
/// guard's cancellation token, which is how that token reaches `ServerDeps` — a
/// handler cannot be written that silently ignores it, because there is no way to
/// build the deps without one.
async fn serve_with_admission<Fut>(
    admission: &crate::http_policy::RequestAdmission,
    server_shutdown: &tokio_util::sync::CancellationToken,
    serve: impl FnOnce(tokio_util::sync::CancellationToken) -> Fut,
) -> Response<ResponseBody>
where
    Fut: std::future::Future<Output = Response<ResponseBody>>,
{
    let Some(guard) = admission.try_admit(server_shutdown) else {
        // Refused rather than queued without bound. The legacy OpenAI error
        // envelope is preserved so existing SDK clients still parse it.
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "The API server active-request quota is exhausted",
            "server_busy",
        );
    };
    serve(guard.cancellation())
        .await
        .map(|body| hold_permit_until_body_ends(body, guard))
}

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
    /// `JoinHandle` of the currently-spawned [`run_accept_loop`] task, so
    /// [`stop_server_core`] can `.await` it — actually confirming the task
    /// observed the `shutdown` notify, broke out of its `select!`, and
    /// dropped its `TcpListener` (freeing the port) — before reporting
    /// "stopped" or letting a caller (e.g. `start_server_core`'s own
    /// restart path) attempt to rebind the same port. Without this, a
    /// same-port restart races the old listener's teardown against the new
    /// bind and fails almost every time with "Address already in use" (see
    /// the review finding this addresses). Never serialized/cloned — this
    /// is pure internal bookkeeping, never part of [`ApiServerStatusPayload`].
    accept_task: Option<tokio::task::JoinHandle<()>>,
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
            accept_task: None,
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
    /// Gates `POST /v1/knowledge/query` — see [`handle_knowledge_query`].
    Knowledge,
    /// Gates `GET /v1/workflows/runs/{id}` — read-only run status, never
    /// run submission. See the module doc comment's "why not submission"
    /// note and [`handle_workflow_run_status`].
    WorkflowRun,
    /// Gates `GET /v1/artifacts/{id}` — see [`handle_artifact_read`].
    ArtifactRead,
    /// Gates `POST /v1/local-apps/{id}/run` — see [`local_apps`] and
    /// [`handle_local_app_run`]. Only ever minted by
    /// [`mint_local_app_token`], never by [`mint_token`] (the generic
    /// create-token flow rejects it outright) — a token carrying this scope
    /// is meaningless without also matching [`TokenEntry::bound_local_app_id`],
    /// which the generic flow never sets. `backends` is always empty on a
    /// token carrying only this scope, so it can never route through
    /// `chat`/`models`/`embeddings` either — see [`mint_local_app_token`].
    LocalAppRun,
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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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
    /// Epoch milliseconds after which [`authenticate`] rejects this token,
    /// or `None` for a token that never expires — the pre-phase-5 default,
    /// preserved by `#[serde(default)]` for every token already on disk.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Set only by [`mint_local_app_token`] — the one Local App id this
    /// token's [`Scope::LocalAppRun`] scope is allowed to run, checked by
    /// [`authenticate_local_app_token`] against the id in the request path.
    /// `None` for every ordinary token minted by [`mint_token`].
    #[serde(default)]
    pub bound_local_app_id: Option<String>,
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
    pub expires_at: Option<u64>,
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
            expires_at: entry.expires_at,
        }
    }
}

/// A revoked token's audit trail — appended to by [`revoke_token_with_state_impl`]
/// and never pruned, so revocation history survives even though the matching
/// [`TokenEntry`] is removed from `ApiServerConfig::tokens` (the hard delete
/// that makes a revoked token stop authenticating immediately). Carries the
/// full pre-revocation snapshot (scopes/backends/created_at/last_used_at),
/// not just `{id, label, revoked_at}` — [`api_server_export_audit`] needs
/// those extra fields to show a revoked token's original grant in the export,
/// not just that it once existed. Never carries `sha256` — same
/// "digest never leaves the Rust side beyond `authenticate`" principle as
/// [`TokenEntryView`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokedTokenEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_used_at: Option<u64>,
    #[serde(default)]
    pub revoked_at: u64,
    /// Snapshot of [`TokenEntry::expires_at`] at revocation time — carried
    /// through so [`TokenAuditEntry`] can still tell an expired grant from a
    /// non-expiring one after the [`TokenEntry`] itself is gone.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// Redacted audit-log row returned by [`api_server_export_audit`] — the
/// union of every still-active [`TokenEntry`] (`revoked_at: None`) and every
/// [`RevokedTokenEntry`] (`revoked_at: Some`), with `sha256`/plaintext never
/// present in either source, so there's no field here that could leak them.
#[derive(Debug, Clone, Serialize)]
pub struct TokenAuditEntry {
    pub id: String,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub backends: Vec<Backend>,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
    pub revoked_at: Option<u64>,
    /// Epoch milliseconds after which this token stopped authenticating, or
    /// `None` if it never expires — mirrors [`TokenEntry::expires_at`] /
    /// [`RevokedTokenEntry::expires_at`] so the audit log can distinguish a
    /// still-live token (`revoked_at: None`, `expires_at` in the future or
    /// `None`) from one that lapsed on its own (`revoked_at: None`,
    /// `expires_at` in the past) instead of rendering both as "Active".
    pub expires_at: Option<u64>,
}

/// Pure merge behind [`api_server_export_audit`] — active tokens first
/// (`revoked_at: None`), then the revoked log — split out so it's testable
/// against a plain [`ApiServerConfig`] value with no file I/O or `AppHandle`.
fn export_audit_impl(config: &ApiServerConfig) -> Vec<TokenAuditEntry> {
    let mut out: Vec<TokenAuditEntry> = config
        .tokens
        .iter()
        .map(|t| TokenAuditEntry {
            id: t.id.clone(),
            label: t.label.clone(),
            scopes: t.scopes.clone(),
            backends: t.backends.clone(),
            created_at: t.created_at,
            last_used_at: t.last_used_at,
            revoked_at: None,
            expires_at: t.expires_at,
        })
        .collect();
    out.extend(config.revoked.iter().map(|r| TokenAuditEntry {
        id: r.id.clone(),
        label: r.label.clone(),
        scopes: r.scopes.clone(),
        backends: r.backends.clone(),
        created_at: r.created_at,
        last_used_at: r.last_used_at,
        revoked_at: Some(r.revoked_at),
        expires_at: r.expires_at,
    }));
    out
}

/// Returns every token's redacted audit trail — active and revoked alike —
/// for the Settings panel's audit-log export. Never includes `sha256` or the
/// plaintext token: [`TokenAuditEntry`] simply has no field for either.
#[tauri::command]
pub fn api_server_export_audit(app: AppHandle) -> Result<Vec<TokenAuditEntry>, String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    Ok(export_audit_impl(&config))
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
    /// Audit trail of revoked tokens — see [`RevokedTokenEntry`]. Never
    /// surfaced through [`ApiServerConfigView`]; only through
    /// [`api_server_export_audit`].
    #[serde(default)]
    pub revoked: Vec<RevokedTokenEntry>,
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
            revoked: Vec::new(),
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
/// `web.rs::settings_file_path`. `pub` so a future `monkey-cli` `api-serve`
/// subcommand (phase 4) can resolve the same path with its own
/// APP_IDENTIFIER, the same config-drift concern the design doc flags.
pub fn config_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base).map_err(|e| {
            format!(
                "Failed to create app data directory {}: {e}",
                base.display()
            )
        })?;
    }
    Ok(base.join(CONFIG_FILE))
}

/// Core load logic, parameterized by path so it needs no `AppHandle` —
/// directly unit-testable and reusable from `monkey-cli`. A missing file (the
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
    let payload = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize api_server.json: {e}"))?;
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
    Providers {
        provider_id: String,
        model_id: String,
    },
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
#[derive(Clone, Default)]
struct TokenAuth {
    id: String,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
    /// See [`TokenEntry::bound_local_app_id`].
    bound_local_app_id: Option<String>,
}

/// The subset of a [`TokenEntry`] needed to authenticate one request —
/// deliberately not the full struct (no `label`/`created_at`), assembled
/// fresh from `api_server.json` in [`build_deps`] on every request so a
/// revoked token stops working immediately without a server restart.
#[derive(Clone, Default)]
struct StoredToken {
    id: String,
    sha256: String,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
    /// See [`TokenEntry::expires_at`] — checked by [`authenticate`].
    expires_at: Option<u64>,
    /// See [`TokenEntry::bound_local_app_id`].
    bound_local_app_id: Option<String>,
}

/// Shared `403` gate for every route (the original five and the phase-5
/// extended three alike) that requires a specific [`Scope`] — same "`None`
/// auth means unrestricted, `Some` must contain the scope" shape every
/// inline `if let Some(auth) = authed { if !auth.scopes.contains(...) }`
/// check above already uses; factored out here specifically so it's
/// directly unit-testable with no `AppHandle` (see the module doc comment on
/// [`handle_extended_request`]'s three routes needing one to actually run,
/// which the scope gate itself must not).
fn require_scope(
    authed: Option<&TokenAuth>,
    scope: Scope,
    label: &str,
) -> Result<(), Response<ResponseBody>> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&scope) {
            return Err(forbidden_response(&format!(
                "This token isn't scoped for `{label}`."
            )));
        }
    }
    Ok(())
}

/// Which of the three phase-5 extended routes (if any) a method+path pair
/// matches — pure and `AppHandle`-free so the routing decision itself is
/// unit-testable independently of the AppHandle-requiring handlers it feeds
/// into. Mirrors the plain `match` [`handle_request`] uses for the original
/// five routes.
#[derive(Debug, PartialEq, Eq)]
enum ExtendedRoute {
    KnowledgeQuery,
    ArtifactRead(String),
    WorkflowRunStatus(String),
    /// `POST /v1/local-apps/{id}/run` — see [`handle_local_app_run`].
    LocalAppRun(String),
    /// `GET /local-apps/{id}` or `GET /local-apps/{id}/{rel_path}` — see
    /// [`handle_local_app_static`]. Unauthenticated: a published Local App's
    /// static page must open in a plain browser tab with no bearer token.
    LocalAppStatic { app_id: String, rel_path: String },
}

fn extended_route_for(method: &Method, path: &str) -> Option<ExtendedRoute> {
    if method == Method::POST && path == "/v1/knowledge/query" {
        return Some(ExtendedRoute::KnowledgeQuery);
    }
    if method == Method::POST {
        if let Some(id) = path
            .strip_prefix("/v1/local-apps/")
            .and_then(|rest| rest.strip_suffix("/run"))
        {
            if !id.is_empty() && !id.contains('/') {
                return Some(ExtendedRoute::LocalAppRun(id.to_string()));
            }
        }
    }
    if method == Method::GET {
        if let Some(id) = path.strip_prefix("/v1/artifacts/") {
            if !id.is_empty() {
                return Some(ExtendedRoute::ArtifactRead(id.to_string()));
            }
        }
        if let Some(run_id) = path.strip_prefix("/v1/workflows/runs/") {
            if !run_id.is_empty() {
                return Some(ExtendedRoute::WorkflowRunStatus(run_id.to_string()));
            }
        }
        if let Some(rest) = path.strip_prefix("/local-apps/") {
            let mut parts = rest.splitn(2, '/');
            let app_id = parts.next().unwrap_or("");
            let rel_path = parts.next().unwrap_or("");
            if !app_id.is_empty() {
                return Some(ExtendedRoute::LocalAppStatic {
                    app_id: app_id.to_string(),
                    rel_path: rel_path.to_string(),
                });
            }
        }
    }
    None
}

/// Everything a single request needs to be routed/authenticated/served,
/// snapshotted fresh per request (cheap: one mutex lock, a couple of clones,
/// and a small JSON file read — see [`build_deps`]) so llama's live
/// port/status/model *and* the current token list are never stale mid-
/// connection. No `AppHandle` here by design — this is what makes
/// [`handle_request`] directly unit-testable and, later, `monkey-cli`-reusable.
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
    /// This request's cancellation token, from its [`http_policy::AdmissionGuard`].
    ///
    /// A field rather than a parameter so no handler signature has to learn that
    /// cancellation exists — handlers already receive `&ServerDeps`.
    ///
    /// **What this is actually for is server shutdown, not client disconnect.**
    /// A disconnecting client is already handled by drop: hyper drops the service
    /// future, which drops the reqwest future with it. Stopping the API server was
    /// the real hole — `stop_server_core` awaits only the accept loop's task, and
    /// every connection is a separate `tokio::spawn` that nothing joins, so
    /// requests it already accepted kept streaming from upstream after the user
    /// pressed Stop and the UI said "stopped". The accept loop's drop-guard
    /// cancels the parent token on exit, and every guard's token is a child of it,
    /// so honouring this is what makes that stop real.
    pub cancel: tokio_util::sync::CancellationToken,
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
    Full::new(bytes.into())
        .map_err(|never: Infallible| match never {})
        .boxed()
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
        .header(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        )
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        )
        .header(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        )
        .body(full_body(Bytes::new()))
        .expect("building a fixed-shape preflight response never fails")
}

/// Stamps `Access-Control-Allow-Origin: *` onto every response this server
/// returns (not just `/v1/*`) — a browser-based client fetching `/health`
/// benefits too, and there's nothing origin-sensitive being protected here
/// (the bearer token, not the browser's same-origin policy, is the actual
/// gate). This is only true because [`authenticate`] always requires a real
/// token for any request carrying an `Origin` header, even when
/// `require_token` is off — see its doc comment.
fn with_cors(mut resp: Response<ResponseBody>) -> Response<ResponseBody> {
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    resp
}

/// Bearer-token check for every route except `/health`/`OPTIONS` preflight.
/// `Ok(None)` means the request may proceed with no further scope/backend
/// restriction (either `require_token` is off, or — not modeled today —
/// a future "admin" token type with no restrictions at all). `Ok(Some(auth))`
/// means the request is authenticated as that specific token, and route
/// handlers must still check `auth.scopes`/`auth.backends`. `Err(response)`
/// is the exact response to return immediately.
fn authenticate(
    deps: &ServerDeps,
    headers: &HeaderMap,
) -> Result<Option<TokenAuth>, Response<ResponseBody>> {
    // `require_token: false` is documented (module doc comment) as an escape
    // hatch for tools that literally can't set a custom header — it was
    // never meant to also hand out unauthenticated access to whatever
    // webpage the user happens to have open in a browser tab. A browser
    // always attaches an `Origin` header on a cross-origin fetch/XHR (and,
    // per the Fetch spec, on most same-origin non-`GET` ones too); a genuine
    // "can't set headers" tool never sends one. So treat a request carrying
    // an `Origin` header as always requiring a real bearer token, regardless
    // of the `require_token` toggle — this is what actually backs up
    // `with_cors`'s "the bearer token is the real gate" reasoning, which
    // this server's wildcard `Access-Control-Allow-Origin: *` otherwise
    // leaves with no gate at all whenever `require_token` is off (the
    // security-review finding this closes: without it, any open browser tab
    // could silently drive `/v1/chat/completions` — including a live,
    // credential-spending provider call — with zero authentication).
    let browser_request = headers.contains_key(header::ORIGIN);
    if !deps.require_token && !browser_request {
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
            // An expired match falls through to the exact same generic
            // "incorrect API key" error below (via `break`, not a distinct
            // early `return Err(...)`) rather than a dedicated "token
            // expired" response — deliberately, so a caller can't use the
            // response to distinguish "this token existed but expired" from
            // "this token never existed at all", the same steady-state
            // "an authentication failure must not describe why via the
            // response" this file already holds for the wrong-token case.
            if let Some(expires_at) = stored.expires_at {
                if now_ms() >= expires_at {
                    break;
                }
            }
            return Ok(Some(TokenAuth {
                id: stored.id.clone(),
                scopes: stored.scopes.clone(),
                backends: stored.backends.clone(),
                bound_local_app_id: stored.bound_local_app_id.clone(),
            }));
        }
    }

    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "Incorrect API key provided. Find the current one in Little Monkey's Settings > API Server.",
        "invalid_api_key",
    ))
}

/// Whether a token (if any) is allowed to see/route to `backend` — split out
/// as a pure, directly unit-testable helper (no I/O), mirroring
/// [`route_backend`]'s style. `None` (no matched token, i.e. `require_token`
/// off) always allows every backend, the same "nothing to restrict" stance
/// every other route's `if let Some(auth) = authed` check already takes.
/// [`handle_models`] uses this to gate each section of the merged listing —
/// see its doc comment for why this must hold there too, not just in
/// `handle_chat_completions`/`handle_embeddings`.
fn backend_visible(authed: Option<&TokenAuth>, backend: Backend) -> bool {
    authed.map_or(true, |auth| auth.backends.contains(&backend))
}

/// `GET /v1/models`. Every section of the merged listing is gated on BOTH
/// the matching `deps.expose_*` global toggle AND (via [`backend_visible`])
/// the token's own `backends` restriction — the same two-independent-gates
/// invariant `handle_chat_completions`/`handle_embeddings` already enforce
/// before ever routing a request to a backend (see the module doc comment).
/// Before this fix, only `scopes` was checked here, so a token deliberately
/// scoped away from `Backend::Providers`/`Backend::Ollama` could still
/// enumerate — and, for providers, trigger a live authenticated network call
/// against — every configured backend via this one route.
async fn handle_models(deps: &ServerDeps, authed: Option<&TokenAuth>) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Models) {
            return forbidden_response("This token isn't scoped for `models`.");
        }
    }

    let mut data = Vec::new();

    if deps.llama_ready && backend_visible(authed, Backend::Local) {
        if let Some(stem) = &deps.llama_model_stem {
            data.push(json!({ "id": stem, "object": "model", "owned_by": "local" }));
        }
    }

    // A skipped capability probe on purpose — `ollama::list_tag_names`
    // fetches only `/api/tags`, never `/api/show`, per the design doc's
    // "Ollama model listing latency" risk note. Gated behind the config's
    // `expose_ollama` toggle: when it's off, `/v1/models` must only ever
    // advertise what will actually serve — see the design doc's "Jan
    // pitfall to avoid" note. Also gated on `Backend::Ollama` visibility —
    // a token not scoped for the `ollama` backend must never see (or cause a
    // request against) it, exactly like `handle_chat_completions` already
    // enforces for that backend.
    if deps.expose_ollama && backend_visible(authed, Backend::Ollama) {
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
    // every other backend. Also gated on `Backend::Providers` visibility —
    // without this, a token scoped away from `providers` could still force a
    // real keychain read + authenticated outbound request to every
    // configured cloud provider just by calling this route (the exact
    // security-review finding this gate closes).
    if deps.expose_providers && backend_visible(authed, Backend::Providers) {
        for provider in &deps.providers {
            let Ok(api_key) = providers::read_key(&provider.id) else {
                continue;
            };
            if let Ok(models) =
                providers::fetch_models(&provider.base_url, &provider.id, &api_key).await
            {
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

async fn handle_chat_completions(
    deps: &ServerDeps,
    authed: Option<&TokenAuth>,
    body: Bytes,
) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Chat) {
            return forbidden_response("This token isn't scoped for `chat`.");
        }
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Invalid JSON body",
                "invalid_request_error",
            )
        }
    };

    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let route = route_model(
        &model,
        deps.llama_ready,
        deps.llama_model_stem.as_deref(),
        &deps.providers,
    );

    if route == ModelRoute::Unknown {
        // Mirrors OpenAI's own wording for a request with no `model`.
        return error_response(
            StatusCode::NOT_FOUND,
            "you must provide a model parameter",
            "model_not_found",
        );
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
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("Unknown model '{model}'"),
            "model_not_found",
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

    let request_builder = match &route {
        ModelRoute::Llama => deps
            .client
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                deps.llama_port
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Ollama => deps
            .client
            .post(format!("{}/v1/chat/completions", deps.ollama_base_url))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Providers {
            provider_id,
            model_id,
        } => {
            // `provider_id` is guaranteed to match an entry in
            // `deps.providers` — `route_model` only ever produces this
            // variant for a known provider id — but a defensive `NOT_FOUND`
            // beats an `unwrap` panic if that invariant is ever broken.
            let Some(base_url) = deps
                .providers
                .iter()
                .find(|p| &p.id == provider_id)
                .map(|p| p.base_url.clone())
            else {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!("Unknown model '{model}'"),
                    "model_not_found",
                );
            };
            let api_key = match providers::read_key(provider_id) {
                Ok(key) => key,
                Err(e) => {
                    return error_response(StatusCode::BAD_GATEWAY, &e, "provider_not_configured")
                }
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
            let request = deps
                .client
                .post(format!("{base_url}/chat/completions"))
                .bearer_auth(&api_key)
                .json(&outgoing);
            providers::add_anthropic_headers(request, provider_id, &api_key)
        }
        ModelRoute::Unknown => unreachable!("handled above"),
    };

    let Some(sent) = unless_cancelled(&deps.cancel, request_builder.send()).await else {
        return cancelled_response();
    };
    let upstream = match sent {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach the upstream model server: {e}"),
                "upstream_unreachable",
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if stream {
            "text/event-stream"
        } else {
            "application/json"
        })
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
            .expect(
                "building a streaming response from an upstream status + content-type never fails",
            )
    } else {
        // Reading the body is where a slow upstream actually spends its time, so
        // cancelling only the send would leave the request running past a stop.
        let Some(read) = unless_cancelled(&deps.cancel, upstream.bytes()).await else {
            return cancelled_response();
        };
        match read {
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
async fn handle_embeddings(
    deps: &ServerDeps,
    authed: Option<&TokenAuth>,
    body: Bytes,
) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Embeddings) {
            return forbidden_response("This token isn't scoped for `embeddings`.");
        }
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Invalid JSON body",
                "invalid_request_error",
            )
        }
    };
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let route = route_model(
        &model,
        deps.llama_ready,
        deps.llama_model_stem.as_deref(),
        &deps.providers,
    );

    if route == ModelRoute::Unknown {
        return error_response(
            StatusCode::NOT_FOUND,
            "you must provide a model parameter",
            "model_not_found",
        );
    }

    if let ModelRoute::Providers { .. } = &route {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Embeddings via a cloud provider aren't supported yet — use a local llama-server model (started with --embeddings) or an Ollama tag.",
            "embeddings_not_supported",
        );
    }

    if route == ModelRoute::Ollama && !deps.expose_ollama {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("Unknown model '{model}'"),
            "model_not_found",
        );
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

    let request = deps
        .client
        .post(&upstream_url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send();
    let Some(sent) = unless_cancelled(&deps.cancel, request).await else {
        return cancelled_response();
    };
    let upstream = match sent {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach the upstream model server: {e}"),
                "upstream_unreachable",
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let Some(read) = unless_cancelled(&deps.cancel, upstream.bytes()).await else {
        return cancelled_response();
    };
    match read {
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

// ---------------------------------------------------------------------
// Phase 5 extended routes (GUI-only — need a real `AppHandle`)
// ---------------------------------------------------------------------

/// `POST /v1/knowledge/query` — thin wrapper around
/// `knowledge_service::knowledge_v2_query`, reused verbatim. Read-only (a
/// hybrid search over an already-indexed stack), so this carries no more
/// risk than `handle_models` already does.
async fn handle_knowledge_query(
    app: &AppHandle,
    authed: Option<&TokenAuth>,
    body: Bytes,
) -> Response<ResponseBody> {
    if let Err(resp) = require_scope(authed, Scope::Knowledge, "knowledge") {
        return resp;
    }

    let request: crate::knowledge_service::KnowledgeQueryRequest =
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid JSON body: {e}"),
                    "invalid_request_error",
                )
            }
        };

    match crate::knowledge_service::knowledge_v2_query(app.clone(), request).await {
        Ok(value) => match serde_json::to_value(&value) {
            Ok(json) => json_response(StatusCode::OK, json),
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode the knowledge query response",
                "internal_error",
            ),
        },
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e, "knowledge_query_failed"),
    }
}

/// `GET /v1/artifacts/{id}` — thin wrapper around
/// `artifact_commands::artifact_blob_read_base64`, reused verbatim. That
/// command already rejects path traversal (ids are content-addressed, never
/// filesystem paths) and caps the decoded size, so this route adds no new
/// surface beyond exposing an existing, already-hardened read over HTTP.
async fn handle_artifact_read(
    app: &AppHandle,
    authed: Option<&TokenAuth>,
    id: String,
) -> Response<ResponseBody> {
    if let Err(resp) = require_scope(authed, Scope::ArtifactRead, "artifact_read") {
        return resp;
    }

    let state = app.state::<AppState>();
    match crate::artifact_commands::artifact_blob_read_base64(app.clone(), state, id) {
        Ok(content) => match serde_json::to_value(&content) {
            Ok(json) => json_response(StatusCode::OK, json),
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode the artifact response",
                "internal_error",
            ),
        },
        Err(e) => error_response(StatusCode::NOT_FOUND, &e, "artifact_not_found"),
    }
}

/// `GET /v1/workflows/runs/{id}` — status only, never submission. See the
/// module doc comment's "why not submission" note for the security
/// reasoning; wraps `run_commands::run_get` verbatim.
async fn handle_workflow_run_status(
    app: &AppHandle,
    authed: Option<&TokenAuth>,
    run_id: String,
) -> Response<ResponseBody> {
    if let Err(resp) = require_scope(authed, Scope::WorkflowRun, "workflow_run") {
        return resp;
    }

    let state = app.state::<AppState>();
    match crate::run_commands::run_get(app.clone(), state, run_id) {
        Ok(Some(run)) => match serde_json::to_value(&run) {
            Ok(json) => json_response(StatusCode::OK, json),
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode the run status response",
                "internal_error",
            ),
        },
        Ok(None) => error_response(StatusCode::NOT_FOUND, "Run not found", "run_not_found"),
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e, "run_lookup_failed"),
    }
}

/// A published Local App's `run` route is a standing invitation for any
/// local process to trigger a scoped recipe execution, so unlike every
/// other route in this file it must always require its own valid, correctly
/// -bound bearer token — regardless of the server's global `require_token`
/// toggle, which exists only as an escape hatch for tools that can't set a
/// custom header on the `chat`/`models`/`embeddings` routes it was designed
/// for. Honoring that toggle here would mean "leave API auth off" also
/// silently hands out unauthenticated recipe-execution triggers to anything
/// on loopback. Returns the matched [`TokenAuth`] only when it both carries
/// [`Scope::LocalAppRun`] *and* its [`TokenEntry::bound_local_app_id`]
/// matches `app_id` from the request path — this pairing is what makes it
/// structurally impossible for a Local App's token to run any recipe other
/// than the one it was minted for.
fn authenticate_local_app_token(
    deps: &ServerDeps,
    headers: &HeaderMap,
    app_id: &str,
) -> Result<TokenAuth, Response<ResponseBody>> {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = provided else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Missing bearer token for this Local App.",
            "invalid_api_key",
        ));
    };
    let digest = sha256_hex(token);
    for stored in &deps.tokens {
        if constant_time_eq(&digest, &stored.sha256) {
            if let Some(expires_at) = stored.expires_at {
                if now_ms() >= expires_at {
                    break;
                }
            }
            if !stored.scopes.contains(&Scope::LocalAppRun)
                || stored.bound_local_app_id.as_deref() != Some(app_id)
            {
                return Err(forbidden_response(
                    "This token isn't scoped to run this Local App.",
                ));
            }
            return Ok(TokenAuth {
                id: stored.id.clone(),
                scopes: stored.scopes.clone(),
                backends: stored.backends.clone(),
                bound_local_app_id: stored.bound_local_app_id.clone(),
            });
        }
    }
    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "Incorrect API key provided for this Local App.",
        "invalid_api_key",
    ))
}

/// `GET /local-apps/{id}` / `GET /local-apps/{id}/{rel_path}` — serves the
/// static page `local_apps::publish_impl` generated, scoped strictly to that
/// one app's own directory. Unauthenticated by design (see
/// [`handle_extended_request`]'s doc comment): opening the link in a plain
/// browser tab must work with no bearer token at all. Path-traversal safety
/// is entirely `local_apps::read_static_file`'s job (canonicalize + verify
/// prefix, the same convention `native_skills.rs` uses).
async fn handle_local_app_static(
    app: &AppHandle,
    app_id: &str,
    rel_path: &str,
) -> Response<ResponseBody> {
    let Ok(app_data_dir) = app.path().app_data_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve the app data directory",
            "internal_error",
        );
    };
    match crate::local_apps::read_static_file(&app_data_dir, app_id, rel_path) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(full_body(bytes))
            .expect("building a static-file response from a fixed status + content-type never fails"),
        Err(_) => not_found_response(),
    }
}

/// `POST /v1/local-apps/{id}/run` — the one and only way a published Local
/// App triggers its bound recipe. Deliberately never executes anything
/// itself: it validates the request, requires a fresh human approval via
/// [`permissions::request_permission`] (the same "Default deny" gate every
/// other new external-write action in this codebase goes through — see
/// `triage.rs`'s `triage_send_draft_impl` for the identical pattern), then
/// emits [`crate::local_apps::LOCAL_APP_RUN_REQUESTED_EVENT`] for the
/// desktop app's own frontend event loop to actually run the recipe
/// (`recipeRunner.ts`'s `runRecipeNow`, tagged with this app's id) and
/// produce its Run Capsule — mirroring how `run_submit` only ever records a
/// ledger entry and leaves live tool execution to that same frontend loop
/// (see this module's doc comment).
async fn handle_local_app_run(
    app: &AppHandle,
    authed: &TokenAuth,
    app_id: String,
    body: Bytes,
) -> Response<ResponseBody> {
    if !crate::local_apps::is_valid_app_id(&app_id) || authed.bound_local_app_id.as_deref() != Some(app_id.as_str()) {
        return not_found_response();
    }
    let Ok(app_data_dir) = app.path().app_data_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve the app data directory",
            "internal_error",
        );
    };
    let config = match crate::local_apps::config_file_path(app)
        .and_then(|path| crate::local_apps::load_config_impl(&path))
    {
        Ok(config) => config,
        Err(e) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e, "internal_error")
        }
    };
    let Some(def) = config.apps.iter().find(|a| a.id == app_id) else {
        return not_found_response();
    };
    if !def.enabled {
        return error_response(
            StatusCode::NOT_FOUND,
            "This Local App has been unpublished.",
            "app_disabled",
        );
    }

    let state = app.state::<AppState>();
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    let recipe = match crate::recipes::resolve_recipe_with_path(
        &def.recipe_name,
        workspace_root.as_deref(),
        &app_data_dir,
    ) {
        Ok((recipe, _path)) => recipe,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &e, "recipe_unavailable");
        }
    };

    let overrides: std::collections::HashMap<String, String> = if body.is_empty() {
        std::collections::HashMap::new()
    } else {
        match serde_json::from_slice(&body) {
            Ok(overrides) => overrides,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid JSON body: {e}"),
                    "invalid_request_error",
                )
            }
        }
    };
    let values = match crate::recipes::resolve_param_values(&recipe, &overrides) {
        Ok(values) => values,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e, "invalid_params"),
    };

    let mut param_summary: Vec<String> = values
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    param_summary.sort();
    let detail = format!(
        "Local App '{}' wants to run recipe '{}' with parameters: {}",
        def.name,
        def.recipe_name,
        if param_summary.is_empty() {
            "(none)".to_string()
        } else {
            param_summary.join(", ")
        }
    );
    if let Err(e) = crate::permissions::request_permission(
        app,
        state.inner(),
        "local_app_run",
        detail,
        None,
        None,
        None,
        Some(&def.name),
    )
    .await
    {
        return forbidden_response(&e);
    }

    let _ = app.emit(
        crate::local_apps::LOCAL_APP_RUN_REQUESTED_EVENT,
        crate::local_apps::LocalAppRunRequestedPayload {
            app_id: app_id.clone(),
            recipe_name: def.recipe_name.clone(),
            params: values,
        },
    );

    json_response(
        StatusCode::ACCEPTED,
        json!({ "status": "accepted", "app_id": app_id }),
    )
}

/// Dispatches the three phase-5 extended routes when `path`/`method` match
/// one (see [`extended_route_for`]), applying the same [`authenticate`] gate
/// [`handle_request`] uses before returning `None` so unmatched requests
/// fall through unchanged. Only ever called with `Some(app)` from
/// [`serve_one_request`] — see the module doc comment on why `monkey-cli`
/// (which calls `serve_one_request` with `None`) never reaches these routes.
async fn handle_extended_request(
    app: &AppHandle,
    deps: &ServerDeps,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<(Response<ResponseBody>, Option<String>)> {
    let route = extended_route_for(method, path)?;

    // These two routes intentionally never go through the generic
    // `authenticate` gate below: `LocalAppStatic` must serve to a plain
    // browser tab with no bearer token at all, and `LocalAppRun` must always
    // require its own correctly-bound bearer token regardless of the
    // server's global `require_token` toggle — see
    // [`authenticate_local_app_token`]'s doc comment for why.
    if let ExtendedRoute::LocalAppStatic { app_id, rel_path } = &route {
        let response = handle_local_app_static(app, app_id, rel_path).await;
        return Some((with_cors(response), None));
    }
    if let ExtendedRoute::LocalAppRun(app_id) = &route {
        let authed = match authenticate_local_app_token(deps, headers, app_id) {
            Ok(authed) => authed,
            Err(response) => return Some((with_cors(response), None)),
        };
        let matched_token_id = Some(authed.id.clone());
        let response = handle_local_app_run(app, &authed, app_id.clone(), body.clone()).await;
        return Some((with_cors(response), matched_token_id));
    }

    let authed = match authenticate(deps, headers) {
        Ok(authed) => authed,
        Err(response) => return Some((with_cors(response), None)),
    };
    let matched_token_id = authed.as_ref().map(|a| a.id.clone());

    let response = match route {
        ExtendedRoute::KnowledgeQuery => {
            handle_knowledge_query(app, authed.as_ref(), body.clone()).await
        }
        ExtendedRoute::ArtifactRead(id) => handle_artifact_read(app, authed.as_ref(), id).await,
        ExtendedRoute::WorkflowRunStatus(run_id) => {
            handle_workflow_run_status(app, authed.as_ref(), run_id).await
        }
        ExtendedRoute::LocalAppRun(_) | ExtendedRoute::LocalAppStatic { .. } => {
            unreachable!("handled above")
        }
    };
    Some((with_cors(response), matched_token_id))
}

/// The core router: a plain `match` on method + path, no framework — see the
/// module doc's security note on why this surface must stay exactly these
/// five routes. Returns the response to send *and*, when a token
/// successfully authenticated the request (whether or not it then passed
/// its scope/backend checks), that token's id — [`serve_one_request`]'s
/// caller uses this to bump `last_used_at` without `handle_request` itself
/// needing any I/O or `AppHandle`.
pub async fn handle_request(
    deps: &ServerDeps,
    req: ServerRequest,
) -> (Response<ResponseBody>, Option<String>) {
    let ServerRequest {
        method,
        path,
        headers,
        body,
    } = req;

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
        (Method::POST, "/v1/chat/completions") => {
            handle_chat_completions(deps, authed.as_ref(), body).await
        }
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
/// `monkey-cli`'s `api-serve` subcommand (design doc phase 4) can build the same
/// catalog from its own `AppHandle`-free custom-provider loader
/// (`providers_cli::load_custom_providers`) without duplicating the
/// preset+custom merge logic. See [`run_cli_server`].
fn provider_catalog_from(custom: Vec<providers::CustomProviderEntry>) -> Vec<ProviderSummary> {
    let mut out: Vec<ProviderSummary> = providers::providers_list_presets()
        .into_iter()
        .map(|p| ProviderSummary {
            id: p.id,
            base_url: p.base_url,
        })
        .collect();
    out.extend(custom.into_iter().map(|c| ProviderSummary {
        id: c.id,
        base_url: c.base_url,
    }));
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
            expires_at: t.expires_at,
            bound_local_app_id: t.bound_local_app_id.clone(),
        })
        .collect()
}

fn build_deps(
    app: &AppHandle,
    runtime: &ServerRuntime,
    cancel: tokio_util::sync::CancellationToken,
) -> ServerDeps {
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
        cancel,
    }
}

/// Probes the managed llama-server process's readiness and reports the
/// model id it advertises — the CLI-context substitute for reading
/// `AppState::llama` in-process (which only exists inside the GUI). Used
/// exclusively by [`run_cli_server`]: `monkey-cli api-serve` runs as its own OS
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
        .and_then(|v| {
            v.get("data")
                .and_then(|d| d.as_array())
                .and_then(|arr| arr.first().cloned())
        })
        .and_then(|entry| {
            entry
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        });

    (true, model_id)
}

/// Streams a real hyper request's body in frame by frame, rejecting it the
/// moment the running total would exceed [`MAX_REQUEST_BODY_BYTES`] — unlike
/// `Incoming::collect()`, this never buffers past the cap, so an oversized
/// body can't force an unbounded allocation before it's rejected (the
/// security-review finding this addresses: `collect()` used to buffer the
/// *entire* body — no matter how large — before `handle_request` had even
/// looked at the Authorization header). A read that fails partway through
/// (client disconnect, malformed chunked encoding) is reported as its own
/// distinct `400 body_read_error`, rather than silently substituting an
/// empty body and letting it fail later as a confusing generic "Invalid JSON
/// body" — a second, independently-reported review finding.
/// Generic over the body type (rather than hardcoded to [`Incoming`]) purely
/// so unit tests can drive it with a synthetic `StreamBody` instead of a real
/// hyper connection — [`serve_one_request`] always calls it with a real
/// `Incoming` and [`MAX_REQUEST_BODY_BYTES`].
async fn read_capped_body<B>(mut body: B, limit: usize) -> Result<Bytes, Response<ResponseBody>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut collected: Vec<u8> = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                if collected.len() + data.len() > limit {
                    return Err(with_cors(error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        &format!("Request body exceeds the {limit}-byte limit."),
                        "request_too_large",
                    )));
                }
                collected.extend_from_slice(data);
            }
            Some(Err(_)) => {
                return Err(with_cors(error_response(
                    StatusCode::BAD_REQUEST,
                    "The request body could not be fully read — the connection was interrupted or the transfer encoding was malformed.",
                    "body_read_error",
                )));
            }
            None => break,
        }
    }
    Ok(Bytes::from(collected))
}

/// Adapts a real hyper request into the `AppHandle`-free [`handle_request`]
/// core: reads the body (capped — see [`read_capped_body`]) and hands
/// everything off unchanged. `app`, when `Some` (the GUI accept loop; never
/// `monkey-cli`, which has none), also gives [`handle_extended_request`] a
/// chance to claim one of the three phase-5 extended routes before falling
/// through to `handle_request`'s original five.
async fn serve_one_request(
    deps: ServerDeps,
    req: Request<Incoming>,
    app: Option<&AppHandle>,
) -> Result<(Response<ResponseBody>, Option<String>), Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let body = match read_capped_body(req.into_body(), MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(response) => return Ok((response, None)),
    };

    if let Some(app) = app {
        if let Some(result) =
            handle_extended_request(app, &deps, &method, &path, &headers, &body).await
        {
            return Ok(result);
        }
    }

    Ok(handle_request(
        &deps,
        ServerRequest {
            method,
            path,
            headers,
            body,
        },
    )
    .await)
}

fn bump_request_count(app: &AppHandle) {
    let state = app.state::<AppState>();
    let payload = {
        let Ok(mut s) = state.api_server.lock() else {
            return;
        };
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
    let Ok(_guard) = state.api_server_config_lock.lock() else {
        return;
    };
    let Ok(mut config) = load_config_impl(path) else {
        return;
    };
    if let Some(entry) = config.tokens.iter_mut().find(|t| t.id == token_id) {
        entry.last_used_at = Some(now_ms());
        let _ = save_config_impl(path, &config);
    }
}

/// Best-effort: a failure to record "last used" (corrupt file, race with a
/// concurrent revoke) never fails the request that's already been served.
fn record_token_used(app: &AppHandle, token_id: &str) {
    let Ok(path) = config_file_path(app) else {
        return;
    };
    record_token_used_with_state(&app.state::<AppState>(), &path, token_id);
}

/// The accept loop. Exits only when `shutdown` is notified — `stop_server_core`
/// is the only caller of that, and it `.await`s this task's `JoinHandle`
/// specifically so it can rely on `listener` having actually been dropped
/// (freeing the port) by the time it returns. Status bookkeeping ("stopped",
/// clearing `shutdown`/`accept_task`) is deliberately NOT done here — it's
/// `stop_server_core`'s job, since it already took both fields out of
/// `ApiServerState` before notifying this loop, and doing it here too would
/// race that same-port-restart concern right back in (see the review finding
/// `ApiServerState::accept_task`'s doc comment addresses).
async fn run_accept_loop(
    app: AppHandle,
    listener: TcpListener,
    shutdown: Arc<Notify>,
    runtime: Arc<ServerRuntime>,
) {
    // Bounded admission, shared with the compatibility listener and with
    // `run_cli_server` — see [`serve_with_admission`], which is the whole rule.
    // Before this, every connection here spawned an unbounded task with no permit
    // and no in-flight accounting.
    //
    // The permit is now held until the response *body* ends rather than until the
    // handler returns, so the bound covers streaming requests. Cancellation is a
    // separate matter and is still not wired: the guard carries a token, and no
    // legacy handler reads it — see [`serve_with_admission`].
    let admission = Arc::new(crate::http_policy::RequestAdmission::new(
        crate::http_policy::MAX_ACTIVE_REQUESTS,
    ));
    // Cancelled when the loop exits, so every in-flight request is torn down
    // rather than outliving the listener that accepted it.
    let server_shutdown = tokio_util::sync::CancellationToken::new();
    let _shutdown_on_exit = server_shutdown.clone().drop_guard();

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
                let admission_for_conn = admission.clone();
                let shutdown_for_conn = server_shutdown.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let app_for_req = app_for_conn.clone();
                        let runtime_for_req = runtime_for_conn.clone();
                        let admission_for_req = admission_for_conn.clone();
                        let shutdown_for_req = shutdown_for_conn.clone();
                        async move {
                            let response = serve_with_admission(
                                &admission_for_req,
                                &shutdown_for_req,
                                |cancel| async move {
                                    // Built inside, so the deps carry this
                                    // request's token; there is no way to
                                    // construct them without one.
                                    let deps =
                                        build_deps(&app_for_req, &runtime_for_req, cancel);
                                    let (resp, matched_token_id) =
                                        match serve_one_request(deps, req, Some(&app_for_req)).await
                                        {
                                            Ok(pair) => pair,
                                            // `serve_one_request`'s error type is
                                            // `Infallible`, so this arm is
                                            // unreachable rather than swallowed.
                                            Err(never) => match never {},
                                        };
                                    if let Some(token_id) = matched_token_id {
                                        record_token_used(&app_for_req, &token_id);
                                    }
                                    bump_request_count(&app_for_req);
                                    resp
                                },
                            )
                            .await;
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        }
    }
    // `listener` drops here as the function returns — the exact event
    // `stop_server_core` is waiting for by awaiting this task's handle.
}

/// Attempts to bind `port` on loopback only. Split out from
/// [`start_server_core`] so the "port already in use" failure path is
/// directly unit-testable without a `#[tauri::command]`/`AppHandle` — see
/// `tests::bind_conflict_surfaces_as_status_error`.
async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port)).await.map_err(|error| {
        crate::http_policy::describe_bind_error(
            crate::http_policy::ListenerRole::LegacyProxy,
            "127.0.0.1",
            port,
            &error,
        )
    })
}

fn record_bind_error(state: &mut ApiServerState, message: String) {
    state.status = "error".to_string();
    state.last_error = Some(message);
    state.shutdown = None;
}

// ---------------------------------------------------------------------
// monkey-cli `api-serve` (design doc phase 4): the SAME routing/proxy core
// (`ServerDeps`/`serve_one_request`/`handle_request`/`bind_listener`/
// `load_config_impl`) the GUI uses, with no `AppHandle`/`AppState` at all —
// only the surrounding bookkeeping differs (stdout/stderr logging instead
// of `apiserver://status` events, an `AtomicU64` instead of
// `AppState::api_server.request_count`, an HTTP probe instead of reading
// `AppState::llama` in-process). `monkey-cli`'s `main.rs` resolves the
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
/// `monkey-cli`'s `main` can print it and exit non-zero exactly like every other
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
    // The same bound the GUI loop has, for the same routes. This loop serves the
    // identical route set through the same `serve_one_request`, so without this
    // every legacy route was reachable with admission control bypassed simply by
    // running `monkey-cli api-serve` — which is what made the GUI loop's "a route
    // on this listener can no longer bypass admission control" untrue.
    //
    // Constructed once, out here: a `RequestAdmission` created inside the
    // connection task would bound each connection separately, which looks correct
    // and bounds nothing.
    let admission = Arc::new(crate::http_policy::RequestAdmission::new(
        crate::http_policy::MAX_ACTIVE_REQUESTS,
    ));
    // This loop owns its process, so its shutdown token is only ever cancelled by
    // the process ending. It exists because a guard's token is derived from one;
    // it is not a claim that `api-serve` has a graceful stop.
    let server_shutdown = tokio_util::sync::CancellationToken::new();

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
        let admission_for_conn = admission.clone();
        let shutdown_for_conn = server_shutdown.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let client = client.clone();
                let config_path = config_path.clone();
                let request_count = request_count.clone();
                let load_custom_providers = load_custom_providers.clone();
                let cli_state = cli_state.clone();
                let admission_for_req = admission_for_conn.clone();
                let shutdown_for_req = shutdown_for_conn.clone();
                async move {
                    let response = serve_with_admission(
                        &admission_for_req,
                        &shutdown_for_req,
                        |cancel| async move {
                            let config = load_config_impl(&config_path).unwrap_or_default();
                            let (llama_ready, llama_model_stem) =
                                probe_llama_server(&client, llama_port).await;
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
                                cancel,
                            };

                            let (resp, matched_token_id) =
                                match serve_one_request(deps, req, None).await {
                                    Ok(pair) => pair,
                                    // `Infallible`: unreachable, not swallowed.
                                    Err(never) => match never {},
                                };
                            let n =
                                request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if let Some(token_id) = &matched_token_id {
                                record_token_used_with_state(&cli_state, &config_path, token_id);
                            }
                            eprintln!(
                                "[api-serve] request #{n} {} -> {}",
                                req_log_hint(matched_token_id.as_deref()),
                                resp.status()
                            );
                            resp
                        },
                    )
                    .await;
                    Ok::<_, Infallible>(response)
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
    // Awaited (not fire-and-forget) — see `ApiServerState::accept_task`'s
    // doc comment: without actually waiting for a previous instance's accept
    // loop to observe the shutdown notify and drop its `TcpListener`, the
    // `bind_listener` call below would race that teardown and fail almost
    // every time on a same-port restart.
    let _ = stop_server_core(app).await;

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

    let accept_task = tokio::spawn(run_accept_loop(app.clone(), listener, shutdown, runtime));
    if let Ok(mut s) = state.api_server.lock() {
        s.accept_task = Some(accept_task);
    }

    Ok(payload)
}

/// Stops the server if running (a no-op, not an error, if it's already
/// stopped). Async — and must stay that way — because it `.await`s the
/// accept loop's `JoinHandle` after notifying it, so the port is
/// *guaranteed* free by the time this returns (see
/// `ApiServerState::accept_task`'s doc comment). Every caller (the
/// `api_server_stop` command, `start_server_core`'s own leading call,
/// `api_server_set_config`'s restart path) awaits this for exactly that
/// reason.
async fn stop_server_core(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    let state = app.state::<AppState>();
    let (shutdown, accept_task) = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
        (s.shutdown.take(), s.accept_task.take())
    };
    if let Some(shutdown) = shutdown {
        shutdown.notify_one();
    }
    if let Some(accept_task) = accept_task {
        // The actual fix: block until the accept loop has broken out of its
        // `select!` and dropped its `TcpListener`, not just until we've
        // asked it to. A plain `notify_one()` with no `.await` here is
        // exactly the bug this addresses — the task may not even have been
        // polled yet when the caller goes on to rebind the same port.
        let _ = accept_task.await;
    }

    let payload = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
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
pub async fn api_server_stop(app: AppHandle) -> Result<ApiServerStatusPayload, String> {
    stop_server_core(&app).await
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
    let (updated, needs_restart) =
        set_config_with_state_impl(state.inner(), &config_file_path(&app)?, config)?;
    if needs_restart {
        stop_server_core(&app).await?;
        start_server_core(&app).await?;
    }
    Ok(ApiServerConfigView::from(&updated))
}

/// Builds a fresh token: `lmk-` + 32 hex chars, plus the [`TokenEntry`] that
/// will be persisted (digest only). Split out from
/// [`create_token_with_state_impl`] so the "plaintext never ends up in the
/// persisted entry" property is testable with no file/lock/`AppState` at all
/// — see `tests::creating_a_token_never_persists_its_plaintext`.
fn mint_token(
    label: &str,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
    expires_at: Option<u64>,
) -> Result<(String, TokenEntry), String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("Label is required".to_string());
    }
    if scopes.is_empty() {
        return Err("Select at least one scope".to_string());
    }
    // `LocalAppRun` is meaningless without `bound_local_app_id`, which this
    // generic flow never sets — see `Scope::LocalAppRun`'s doc comment.
    // Only `mint_local_app_token` may ever produce a token carrying it.
    if scopes.contains(&Scope::LocalAppRun) {
        return Err(
            "Scope 'local_app_run' can only be granted by publishing a Local App".to_string(),
        );
    }
    if backends.is_empty() {
        return Err("Select at least one backend".to_string());
    }
    if let Some(expires_at) = expires_at {
        if expires_at <= now_ms() {
            return Err("Expiration must be in the future".to_string());
        }
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
        expires_at,
        bound_local_app_id: None,
    };
    Ok((token, entry))
}

/// Mints a token that can do exactly one thing: `POST` the `run` route for
/// one specific published Local App — see [`Scope::LocalAppRun`]'s doc
/// comment. `backends` is always empty: `Backend::{Local,Ollama,Providers}`
/// only ever gate the model-proxying routes (`chat`/`models`/`embeddings`),
/// and a Local App token must never be able to reach any of them — an empty
/// list makes [`backend_visible`]'s check fail closed for every one of them,
/// independent of `scopes`.
pub fn mint_local_app_token(label: &str, bound_local_app_id: &str) -> (String, TokenEntry) {
    let token = generate_token();
    let entry = TokenEntry {
        id: Uuid::new_v4().to_string(),
        label: label.to_string(),
        sha256: sha256_hex(&token),
        scopes: vec![Scope::LocalAppRun],
        backends: Vec::new(),
        created_at: now_ms(),
        last_used_at: None,
        expires_at: None,
        bound_local_app_id: Some(bound_local_app_id.to_string()),
    };
    (token, entry)
}

/// Locked read-modify-write persistence for [`mint_local_app_token`] — same
/// shape as [`create_token_with_state_impl`], called from
/// `local_apps::publish_impl` rather than a Tauri command directly (Local
/// Apps have their own `local_apps_publish` command, which mints this token
/// as one step of a larger operation).
pub fn create_local_app_token_with_state(
    state: &AppState,
    path: &Path,
    label: &str,
    bound_local_app_id: &str,
) -> Result<(String, TokenEntry), String> {
    let (token, entry) = mint_local_app_token(label, bound_local_app_id);
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    config.tokens.push(entry.clone());
    save_config_impl(path, &config)?;
    Ok((token, entry))
}

fn create_token_with_state_impl(
    state: &AppState,
    path: &Path,
    label: &str,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
    expires_at: Option<u64>,
) -> Result<(String, TokenEntry), String> {
    let (token, entry) = mint_token(label, scopes, backends, expires_at)?;
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    config.tokens.push(entry.clone());
    save_config_impl(path, &config)?;
    Ok((token, entry))
}

/// Removes the token from `config.tokens` (the hard delete that makes it
/// stop authenticating immediately — see [`authenticate`]) and appends its
/// pre-revocation snapshot to `config.revoked` (the audit trail that
/// survives — see [`RevokedTokenEntry`]) in the same locked read-modify-write
/// cycle, so the two never drift apart.
pub(crate) fn revoke_token_with_state_impl(
    state: &AppState,
    path: &Path,
    id: &str,
) -> Result<(), String> {
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    let Some(index) = config.tokens.iter().position(|t| t.id == id) else {
        return Err(format!("Unknown token '{id}'"));
    };
    let removed = config.tokens.remove(index);
    config.revoked.push(RevokedTokenEntry {
        id: removed.id,
        label: removed.label,
        scopes: removed.scopes,
        backends: removed.backends,
        created_at: removed.created_at,
        last_used_at: removed.last_used_at,
        revoked_at: now_ms(),
        expires_at: removed.expires_at,
    });
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
    expires_at: Option<u64>,
) -> Result<CreateTokenResult, String> {
    let (token, entry) = create_token_with_state_impl(
        state.inner(),
        &config_file_path(&app)?,
        &label,
        scopes,
        backends,
        expires_at,
    )?;
    Ok(CreateTokenResult {
        token,
        entry: TokenEntryView::from(&entry),
    })
}

#[tauri::command]
pub fn api_server_revoke_token(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
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
        ProviderSummary {
            id: id.to_string(),
            base_url: base_url.to_string(),
        }
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
            providers: vec![
                test_provider("openai", "https://api.openai.com/v1"),
                test_provider("anthropic", "https://api.anthropic.com/v1"),
            ],
            tokens: Vec::new(),
            client: reqwest::Client::new(),
            // Never cancelled, so every existing test keeps asserting the
            // uncancelled path. The cancellation tests build their own token.
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// `test_deps` with a token the caller controls, for the cancellation paths.
    fn test_deps_cancelled_by(
        ollama_base_url: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> ServerDeps {
        ServerDeps {
            cancel,
            ..test_deps(ollama_base_url)
        }
    }

    fn stored_token(
        id: &str,
        plaintext: &str,
        scopes: Vec<Scope>,
        backends: Vec<Backend>,
    ) -> StoredToken {
        StoredToken {
            id: id.to_string(),
            sha256: sha256_hex(plaintext),
            scopes,
            backends,
            expires_at: None,
            bound_local_app_id: None,
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

    fn with_bearer(mut req: ServerRequest, token: &str) -> ServerRequest {
        req.headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        req
    }

    async fn body_bytes(resp: Response<ResponseBody>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    /// A body that yields its frame only when the test releases it, so a response
    /// can be "returned but still streaming" — the state the old `drop(guard)`
    /// mishandled, and the state a shutdown has to cut without looking clean.
    /// Shared by the `admission` and `cancellation` modules, which assert opposite
    /// halves of the same lifetime.
    fn gated_body(release: tokio::sync::oneshot::Receiver<()>) -> ResponseBody {
        let stream = futures_util::stream::unfold(Some(release), |release| async move {
            let release = release?;
            let _ = release.await;
            Some((
                Ok::<Frame<Bytes>, BoxError>(Frame::data(Bytes::from_static(b"data: [DONE]\n\n"))),
                None,
            ))
        });
        BodyExt::boxed(StreamBody::new(stream))
    }

    fn temp_config_path() -> PathBuf {
        // Nanos alone can collide across parallel test threads — the atomic
        // counter guarantees uniqueness within the process (same idiom as
        // `prompts.rs::tests::temp_file`).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_api_server_test_{}_{}_{}.json",
            std::process::id(),
            n,
            nanos
        ))
    }

    fn no_providers() -> Vec<ProviderSummary> {
        Vec::new()
    }

    fn two_providers() -> Vec<ProviderSummary> {
        vec![
            test_provider("openai", "https://api.openai.com/v1"),
            test_provider("anthropic", "https://api.anthropic.com/v1"),
        ]
    }

    #[test]
    fn route_model_matches_llama_exactly() {
        assert_eq!(
            route_model(
                "qwen2.5-7b-instruct",
                true,
                Some("qwen2.5-7b-instruct"),
                &no_providers()
            ),
            ModelRoute::Llama
        );
    }

    #[test]
    fn route_model_falls_back_to_ollama_for_any_other_nonempty_id() {
        assert_eq!(
            route_model(
                "llama3.1:8b",
                true,
                Some("qwen2.5-7b-instruct"),
                &no_providers()
            ),
            ModelRoute::Ollama
        );
        // Even when llama isn't ready, a non-empty id is assumed to be an
        // Ollama tag — Ollama is the source of truth for whether it exists.
        assert_eq!(
            route_model(
                "qwen2.5-7b-instruct",
                false,
                Some("qwen2.5-7b-instruct"),
                &no_providers()
            ),
            ModelRoute::Ollama
        );
        assert_eq!(
            route_model("anything", true, None, &no_providers()),
            ModelRoute::Ollama
        );
    }

    #[test]
    fn route_model_is_unknown_only_when_blank() {
        assert_eq!(
            route_model("", true, Some("qwen2.5-7b-instruct"), &no_providers()),
            ModelRoute::Unknown
        );
        assert_eq!(
            route_model("   ", true, Some("qwen2.5-7b-instruct"), &no_providers()),
            ModelRoute::Unknown
        );
    }

    #[test]
    fn route_model_routes_a_known_provider_prefixed_id_to_providers() {
        assert_eq!(
            route_model(
                "openai/gpt-4o",
                true,
                Some("qwen2.5-7b-instruct"),
                &two_providers()
            ),
            ModelRoute::Providers {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string()
            }
        );
        assert_eq!(
            route_model("anthropic/claude-opus-4-8", false, None, &two_providers()),
            ModelRoute::Providers {
                provider_id: "anthropic".to_string(),
                model_id: "claude-opus-4-8".to_string()
            }
        );
    }

    #[test]
    fn route_model_falls_back_to_ollama_for_a_slash_id_with_an_unknown_provider_prefix() {
        // "library/llama3" isn't a configured provider id, so it's treated
        // as an Ollama tag (Ollama namespaced tags can themselves contain a
        // slash) — exactly the design doc's "otherwise treat as Ollama tag"
        // fallback.
        assert_eq!(
            route_model(
                "library/llama3",
                true,
                Some("qwen2.5-7b-instruct"),
                &two_providers()
            ),
            ModelRoute::Ollama
        );
    }

    #[test]
    fn route_backend_maps_every_known_route_but_not_unknown() {
        assert_eq!(route_backend(&ModelRoute::Llama), Some(Backend::Local));
        assert_eq!(route_backend(&ModelRoute::Ollama), Some(Backend::Ollama));
        assert_eq!(
            route_backend(&ModelRoute::Providers {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string()
            }),
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
        assert!(a
            .chars()
            .skip(TOKEN_PREFIX.len())
            .all(|c| c.is_ascii_hexdigit()));
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
        last_byte_flipped.replace_range(
            last..last + 1,
            if &base[last..last + 1] == "0" {
                "1"
            } else {
                "0"
            },
        );

        assert!(!constant_time_eq(&base, &first_byte_flipped));
        assert!(!constant_time_eq(&base, &last_byte_flipped));
        assert!(constant_time_eq(&base, &base.clone()));
    }

    #[tokio::test]
    async fn health_requires_no_token_even_when_auth_is_on() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "t1",
            "lmk-real-token",
            vec![Scope::Chat],
            vec![Backend::Local],
        )];

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
        deps.tokens = vec![stored_token(
            "t1",
            "lmk-real-token",
            vec![Scope::Models],
            vec![Backend::Local, Backend::Ollama],
        )];

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
        deps.tokens = vec![stored_token(
            "tok-1",
            "lmk-real-token",
            vec![Scope::Models],
            vec![Backend::Local, Backend::Ollama],
        )];

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
        deps.tokens = vec![stored_token(
            "tok-models-only",
            "lmk-models-only",
            vec![Scope::Models],
            vec![Backend::Local, Backend::Ollama],
        )];

        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"qwen2.5-7b-instruct"}"#),
            "lmk-models-only",
        );
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
        deps.tokens = vec![stored_token(
            "tok-local-only",
            "lmk-local-only",
            vec![Scope::Chat],
            vec![Backend::Local],
        )];

        // "llama3.1:8b" isn't the ready llama stem, so `route_model` sends
        // it to Ollama — a token scoped to `local` only must be rejected.
        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#),
            "lmk-local-only",
        );
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn token_scoped_to_the_matching_backend_is_accepted() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-ollama",
            "lmk-ollama-scoped",
            vec![Scope::Chat],
            vec![Backend::Ollama],
        )];

        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#),
            "lmk-ollama-scoped",
        );
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
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/chat/completions", r#"{"messages":[]}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_with_invalid_json_returns_400() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) =
            handle_request(&deps, post_request("/v1/chat/completions", "not json")).await;
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
        assert!(data
            .iter()
            .any(|m| m["id"] == "qwen2.5-7b-instruct" && m["owned_by"] == "local"));
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
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_404s_for_a_provider_routed_model_when_expose_providers_is_off() {
        // `test_deps` defaults `expose_providers` to `false` — a
        // provider-prefixed id must 404 exactly like an unexposed Ollama tag,
        // never silently proxy anyway.
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/chat/completions", r#"{"model":"openai/gpt-4o"}"#),
        )
        .await;
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
    async fn token_without_providers_backend_is_rejected_even_when_expose_providers_is_globally_on()
    {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-no-providers",
            "lmk-no-providers",
            vec![Scope::Chat],
            vec![Backend::Local, Backend::Ollama],
        )];

        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"openai/gpt-4o"}"#),
            "lmk-no-providers",
        );
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
        deps.providers = vec![test_provider(
            "zzz-test-provider-no-key",
            "https://example.invalid/v1",
        )];
        deps.tokens = vec![stored_token(
            "tok-with-providers",
            "lmk-with-providers",
            vec![Scope::Chat],
            vec![Backend::Providers],
        )];

        let req = with_bearer(
            post_request(
                "/v1/chat/completions",
                r#"{"model":"zzz-test-provider-no-key/some-model"}"#,
            ),
            "lmk-with-providers",
        );
        let (resp, matched) = handle_request(&deps, req).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "no key is configured for this fabricated provider id, so it must fail as 'not configured', not silently succeed");
        assert_eq!(matched.as_deref(), Some("tok-with-providers"));
    }

    #[tokio::test]
    async fn chat_completions_provider_route_reports_provider_not_configured_with_no_token_required(
    ) {
        // A provider id with no saved key deterministically 502s before ever
        // sending a request — this only exercises the routing decision (and
        // that it's reachable with `require_token: false`, unlike the
        // token-scoped variant above), not the actual proxying, which needs
        // a real network call to a mock upstream to exercise.
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.providers = vec![test_provider(
            "zzz-test-provider-no-key",
            "https://example.invalid/v1",
        )];

        let (resp, _) = handle_request(
            &deps,
            post_request(
                "/v1/chat/completions",
                r#"{"model":"zzz-test-provider-no-key/some-model"}"#,
            ),
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
        deps.providers = vec![test_provider(
            "zzz-test-provider-no-key",
            "https://example.invalid/v1",
        )];

        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data
            .iter()
            .all(|m| m["owned_by"] != "zzz-test-provider-no-key"));
    }

    #[tokio::test]
    async fn embeddings_501s_when_llama_wasnt_started_with_embeddings() {
        // `test_deps` defaults `llama_embeddings_enabled` to `false`.
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#),
        )
        .await;
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
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        handle.join().unwrap();
    }

    #[tokio::test]
    async fn embeddings_501s_for_a_provider_routed_model() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        let (resp, _) = handle_request(
            &deps,
            post_request(
                "/v1/embeddings",
                r#"{"model":"openai/text-embedding-3-small"}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "embeddings_not_supported");
    }

    #[tokio::test]
    async fn embeddings_requires_the_embeddings_scope() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-chat-only",
            "lmk-chat-only",
            vec![Scope::Chat],
            vec![Backend::Local, Backend::Ollama],
        )];

        let req = with_bearer(
            post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#),
            "lmk-chat-only",
        );
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
        deps.tokens = vec![stored_token(
            "t1",
            "lmk-real-token",
            vec![Scope::Chat],
            vec![Backend::Local],
        )];

        let req = ServerRequest {
            method: Method::OPTIONS,
            path: "/v1/chat/completions".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
        assert!(resp.headers().get("access-control-allow-methods").is_some());
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn every_response_carries_the_cors_allow_origin_header() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, get_request("/health")).await;
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );

        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );

        let (resp, _) = handle_request(&deps, get_request("/v1/nope")).await;
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
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
        let canned: &[u8] =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";

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
        let (resp, _) = handle_request(
            &deps,
            post_request(
                "/v1/chat/completions",
                r#"{"model":"llama3.1:8b","stream":true}"#,
            ),
        )
        .await;
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
        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#),
        )
        .await;
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
        let listener = bind_listener(0)
            .await
            .expect("binding port 0 (OS-assigned) should always succeed");
        assert!(listener.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn creating_a_token_never_persists_its_plaintext() {
        let (token, entry) =
            mint_token("CI", vec![Scope::Chat], vec![Backend::Local], None).unwrap();
        assert_ne!(
            entry.sha256, token,
            "the persisted entry must never contain the plaintext token"
        );
        assert_eq!(entry.sha256, sha256_hex(&token));

        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains(&token),
            "serialized TokenEntry must never contain the plaintext token"
        );
    }

    #[test]
    fn mint_token_rejects_blank_label_or_empty_scopes_or_backends() {
        assert!(mint_token("", vec![Scope::Chat], vec![Backend::Local], None).is_err());
        assert!(mint_token("   ", vec![Scope::Chat], vec![Backend::Local], None).is_err());
        assert!(mint_token("ok", vec![], vec![Backend::Local], None).is_err());
        assert!(mint_token("ok", vec![Scope::Chat], vec![], None).is_err());
        assert!(mint_token("ok", vec![Scope::Chat], vec![Backend::Local], None).is_ok());
    }

    #[test]
    fn mint_token_rejects_an_expiration_that_is_not_in_the_future() {
        assert!(mint_token(
            "ok",
            vec![Scope::Chat],
            vec![Backend::Local],
            Some(now_ms().saturating_sub(1_000)),
        )
        .is_err());
        assert!(mint_token(
            "ok",
            vec![Scope::Chat],
            vec![Backend::Local],
            Some(now_ms() + 1_000_000),
        )
        .is_ok());
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
            expires_at: None,
        
            ..Default::default()
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
            expires_at: None,
            ..Default::default()
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

        let view = ApiServerConfigView {
            port: 1234,
            autostart: false,
            require_token: true,
            expose_ollama: true,
            expose_providers: false,
        };
        let (_, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
        assert!(!needs_restart, "server is stopped — no restart needed");

        {
            let mut s = state.api_server.lock().unwrap();
            s.status = "running".to_string();
        }
        let view = ApiServerConfigView {
            port: 5555,
            autostart: false,
            require_token: true,
            expose_ollama: true,
            expose_providers: false,
        };
        let (updated, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
        assert!(
            needs_restart,
            "server is running — a config change must trigger a restart"
        );
        assert_eq!(updated.port, 5555);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_rejects_a_zero_port() {
        let path = temp_config_path();
        let state = AppState::default();
        let view = ApiServerConfigView {
            port: 0,
            autostart: false,
            require_token: true,
            expose_ollama: true,
            expose_providers: false,
        };
        assert!(set_config_with_state_impl(&state, &path, view).is_err());
    }

    #[test]
    fn create_and_revoke_token_round_trip_through_the_config_file() {
        let path = temp_config_path();
        let state = AppState::default();

        let (token, entry) = create_token_with_state_impl(
            &state,
            &path,
            "My IDE",
            vec![Scope::Chat, Scope::Models],
            vec![Backend::Local],
            None,
        )
        .unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));

        let loaded = load_config_impl(&path).unwrap();
        assert_eq!(loaded.tokens.len(), 1);
        assert_eq!(loaded.tokens[0].id, entry.id);
        assert_eq!(loaded.tokens[0].sha256, sha256_hex(&token));

        revoke_token_with_state_impl(&state, &path, &entry.id).unwrap();
        let loaded = load_config_impl(&path).unwrap();
        assert!(loaded.tokens.is_empty());
        assert_eq!(
            loaded.revoked.len(),
            1,
            "revocation must append an audit-trail entry, not just delete the token"
        );
        assert_eq!(loaded.revoked[0].id, entry.id);
        assert_eq!(loaded.revoked[0].label, "My IDE");

        assert!(
            revoke_token_with_state_impl(&state, &path, &entry.id).is_err(),
            "revoking an already-gone id must error, not silently succeed"
        );

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
            expires_at: None,
            ..Default::default()
        });
        config.tokens.push(TokenEntry {
            id: "b".to_string(),
            label: "B".to_string(),
            sha256: sha256_hex("lmk-b"),
            scopes: vec![],
            backends: vec![],
            created_at: 1,
            last_used_at: None,
            expires_at: None,
            ..Default::default()
        });
        save_config_impl(&path, &config).unwrap();

        let state = AppState::default();
        record_token_used_with_state(&state, &path, "b");

        let reloaded = load_config_impl(&path).unwrap();
        assert!(reloaded
            .tokens
            .iter()
            .find(|t| t.id == "a")
            .unwrap()
            .last_used_at
            .is_none());
        assert!(reloaded
            .tokens
            .iter()
            .find(|t| t.id == "b")
            .unwrap()
            .last_used_at
            .is_some());

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
    async fn config_triggered_restart_onto_an_already_taken_port_surfaces_status_error_without_hanging(
    ) {
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

        let bind_result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            bind_listener(conflicting_port),
        )
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
    // Phase 4: monkey-cli `api-serve` reuse (`provider_catalog_from`,
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
        assert!(catalog
            .iter()
            .any(|p| p.id == "my-local-router" && p.base_url == "http://127.0.0.1:9999/v1"));
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
            expires_at: None,
            ..Default::default()
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
                let body =
                    r#"{"object":"list","data":[{"id":"qwen2.5-7b-instruct","object":"model"}]}"#;
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
    /// still surface a conflicting port as an `Err` (so `monkey-cli`'s `fail()`
    /// prints it and exits non-zero) rather than hanging or panicking —
    /// mirrors `config_triggered_restart_onto_an_already_taken_port_...`
    /// above, but through the actual public entry point `monkey-cli` calls.
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

    // -------------------------------------------------------------
    // Phase 5: expiry, revocation audit trail, extended-route scopes
    // -------------------------------------------------------------

    fn stored_token_expiring(
        id: &str,
        plaintext: &str,
        scopes: Vec<Scope>,
        backends: Vec<Backend>,
        expires_at: Option<u64>,
    ) -> StoredToken {
        let mut token = stored_token(id, plaintext, scopes, backends);
        token.expires_at = expires_at;
        token
    }

    #[tokio::test]
    async fn authenticate_rejects_an_expired_token_with_the_same_generic_error_as_an_unknown_one() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token_expiring(
            "tok-expired",
            "lmk-expired",
            vec![Scope::Chat],
            vec![Backend::Local],
            Some(now_ms().saturating_sub(1_000)),
        )];

        let req = with_bearer(get_request("/v1/models"), "lmk-expired");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(matched.is_none());

        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let expired_message = value["error"]["message"].as_str().unwrap().to_string();

        // Same response body as a token that was never minted at all — an
        // expired token must not be distinguishable from an unknown one.
        let req = with_bearer(get_request("/v1/models"), "lmk-never-existed");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(matched.is_none());
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["message"].as_str().unwrap(), expired_message);
    }

    #[tokio::test]
    async fn authenticate_accepts_a_token_whose_expiry_is_still_in_the_future() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token_expiring(
            "tok-not-yet-expired",
            "lmk-not-yet-expired",
            vec![Scope::Models],
            vec![Backend::Local],
            Some(now_ms() + 1_000_000),
        )];

        let req = with_bearer(get_request("/v1/models"), "lmk-not-yet-expired");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(matched.as_deref(), Some("tok-not-yet-expired"));
    }

    #[test]
    fn revoked_token_no_longer_authenticates_while_its_audit_trail_survives() {
        let path = temp_config_path();
        let state = AppState::default();

        let (token, entry) = create_token_with_state_impl(
            &state,
            &path,
            "Revoke me",
            vec![Scope::Chat],
            vec![Backend::Local],
            None,
        )
        .unwrap();

        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = tokens_from_config(&load_config_impl(&path).unwrap());
        assert!(
            authenticate(&deps, &with_bearer(get_request("/"), &token).headers).is_ok(),
            "the token must authenticate before it's revoked"
        );

        revoke_token_with_state_impl(&state, &path, &entry.id).unwrap();

        deps.tokens = tokens_from_config(&load_config_impl(&path).unwrap());
        assert!(
            authenticate(&deps, &with_bearer(get_request("/"), &token).headers).is_err(),
            "a revoked token must be denied, not silently accepted"
        );

        let audit = export_audit_impl(&load_config_impl(&path).unwrap());
        let revoked_row = audit.iter().find(|row| row.id == entry.id).unwrap();
        assert!(revoked_row.revoked_at.is_some());
        assert_eq!(revoked_row.label, "Revoke me");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_audit_never_includes_the_digest_or_plaintext_for_active_or_revoked_tokens() {
        let mut config = ApiServerConfig::default();
        let plaintext = "lmk-super-secret-value";
        config.tokens.push(TokenEntry {
            id: "active-1".to_string(),
            label: "Active".to_string(),
            sha256: sha256_hex(plaintext),
            scopes: vec![Scope::Chat],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
            expires_at: None,
            ..Default::default()
        });
        config.revoked.push(RevokedTokenEntry {
            id: "revoked-1".to_string(),
            label: "Revoked".to_string(),
            scopes: vec![Scope::Knowledge],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
            revoked_at: 2,
            expires_at: None,
        });

        let audit = export_audit_impl(&config);
        assert_eq!(audit.len(), 2);

        let json = serde_json::to_string(&audit).unwrap();
        assert!(!json.contains("sha256"));
        assert!(!json.contains(plaintext));
        assert!(!json.contains(&sha256_hex(plaintext)));

        let active_row = audit.iter().find(|row| row.id == "active-1").unwrap();
        assert!(active_row.revoked_at.is_none());
        let revoked_row = audit.iter().find(|row| row.id == "revoked-1").unwrap();
        assert_eq!(revoked_row.revoked_at, Some(2));
    }

    #[test]
    fn export_audit_carries_expires_at_so_an_expired_but_unrevoked_token_is_distinguishable_from_a_genuinely_active_one(
    ) {
        let mut config = ApiServerConfig::default();
        config.tokens.push(TokenEntry {
            id: "expired-1".to_string(),
            label: "Expired but not revoked".to_string(),
            sha256: sha256_hex("lmk-expired-value"),
            scopes: vec![Scope::Chat],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
            expires_at: Some(now_ms() - 1_000),
            ..Default::default()
        });
        config.tokens.push(TokenEntry {
            id: "active-2".to_string(),
            label: "Genuinely active".to_string(),
            sha256: sha256_hex("lmk-active-value"),
            scopes: vec![Scope::Chat],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
            expires_at: None,
            ..Default::default()
        });

        let audit = export_audit_impl(&config);
        let expired_row = audit.iter().find(|row| row.id == "expired-1").unwrap();
        assert!(expired_row.revoked_at.is_none());
        assert!(expired_row.expires_at.is_some_and(|ms| ms < now_ms()));

        let active_row = audit.iter().find(|row| row.id == "active-2").unwrap();
        assert!(active_row.revoked_at.is_none());
        assert!(active_row.expires_at.is_none());
    }

    #[test]
    fn require_scope_rejects_a_chat_only_token_on_a_knowledge_gated_route() {
        let chat_only = TokenAuth {
            id: "tok-chat-only".to_string(),
            scopes: vec![Scope::Chat],
            backends: vec![Backend::Local],
            ..Default::default()
        };
        assert!(require_scope(Some(&chat_only), Scope::Knowledge, "knowledge").is_err());
        assert!(require_scope(Some(&chat_only), Scope::WorkflowRun, "workflow_run").is_err());
        assert!(require_scope(Some(&chat_only), Scope::ArtifactRead, "artifact_read").is_err());
        // Still allowed for the scope it actually carries.
        assert!(require_scope(Some(&chat_only), Scope::Chat, "chat").is_ok());
    }

    #[test]
    fn require_scope_accepts_a_token_carrying_the_new_phase_5_scopes() {
        let full = TokenAuth {
            id: "tok-full".to_string(),
            scopes: vec![Scope::Knowledge, Scope::WorkflowRun, Scope::ArtifactRead],
            backends: vec![Backend::Local],
            ..Default::default()
        };
        assert!(require_scope(Some(&full), Scope::Knowledge, "knowledge").is_ok());
        assert!(require_scope(Some(&full), Scope::WorkflowRun, "workflow_run").is_ok());
        assert!(require_scope(Some(&full), Scope::ArtifactRead, "artifact_read").is_ok());
    }

    #[test]
    fn require_scope_allows_everything_when_no_token_is_matched() {
        // Mirrors `backend_visible`'s "None auth means unrestricted" rule —
        // only reached when `require_token` is off and no bearer was sent.
        assert!(require_scope(None, Scope::Knowledge, "knowledge").is_ok());
        assert!(require_scope(None, Scope::WorkflowRun, "workflow_run").is_ok());
        assert!(require_scope(None, Scope::ArtifactRead, "artifact_read").is_ok());
    }

    #[test]
    fn extended_route_for_matches_exactly_the_three_phase_5_routes() {
        assert_eq!(
            extended_route_for(&Method::POST, "/v1/knowledge/query"),
            Some(ExtendedRoute::KnowledgeQuery)
        );
        assert_eq!(
            extended_route_for(&Method::GET, "/v1/artifacts/abc123"),
            Some(ExtendedRoute::ArtifactRead("abc123".to_string()))
        );
        assert_eq!(
            extended_route_for(&Method::GET, "/v1/workflows/runs/run-1"),
            Some(ExtendedRoute::WorkflowRunStatus("run-1".to_string()))
        );
        // Wrong method for the knowledge route, empty ids, and unrelated
        // paths must all fall through so `handle_request`'s original five
        // routes (or the final 404) still get a chance at them.
        assert_eq!(extended_route_for(&Method::GET, "/v1/knowledge/query"), None);
        assert_eq!(extended_route_for(&Method::GET, "/v1/artifacts/"), None);
        assert_eq!(extended_route_for(&Method::GET, "/v1/workflows/runs/"), None);
        assert_eq!(extended_route_for(&Method::GET, "/v1/models"), None);
    }

    #[test]
    fn extended_route_for_matches_the_local_app_run_and_static_routes() {
        assert_eq!(
            extended_route_for(&Method::POST, "/v1/local-apps/app-1/run"),
            Some(ExtendedRoute::LocalAppRun("app-1".to_string()))
        );
        assert_eq!(
            extended_route_for(&Method::GET, "/local-apps/app-1"),
            Some(ExtendedRoute::LocalAppStatic {
                app_id: "app-1".to_string(),
                rel_path: String::new()
            })
        );
        assert_eq!(
            extended_route_for(&Method::GET, "/local-apps/app-1/index.html"),
            Some(ExtendedRoute::LocalAppStatic {
                app_id: "app-1".to_string(),
                rel_path: "index.html".to_string()
            })
        );
        // Wrong method, a blank id, and a run path with an extra segment
        // must all fall through instead of matching.
        assert_eq!(extended_route_for(&Method::GET, "/v1/local-apps/app-1/run"), None);
        assert_eq!(extended_route_for(&Method::POST, "/v1/local-apps//run"), None);
        assert_eq!(extended_route_for(&Method::GET, "/local-apps/"), None);
    }

    // -------------------------------------------------------------
    // Local App Builder (ROADMAP.md, Phase 3): scoped-token enforcement
    // -------------------------------------------------------------

    fn local_app_stored_token(id: &str, plaintext: &str, bound_local_app_id: &str) -> StoredToken {
        StoredToken {
            id: id.to_string(),
            sha256: sha256_hex(plaintext),
            scopes: vec![Scope::LocalAppRun],
            backends: Vec::new(),
            expires_at: None,
            bound_local_app_id: Some(bound_local_app_id.to_string()),
        }
    }

    #[test]
    fn mint_local_app_token_produces_a_scope_and_binding_that_cannot_reach_anything_else() {
        let (_token, entry) = mint_local_app_token("Local App: nightly-audit", "app-1");
        assert_eq!(entry.scopes, vec![Scope::LocalAppRun]);
        assert!(entry.backends.is_empty());
        assert_eq!(entry.bound_local_app_id.as_deref(), Some("app-1"));
    }

    #[test]
    fn mint_token_rejects_the_local_app_run_scope_from_the_generic_create_token_flow() {
        let result = mint_token(
            "Manually crafted",
            vec![Scope::LocalAppRun],
            vec![Backend::Local],
            None,
        );
        assert!(result.unwrap_err().contains("local_app_run"));
    }

    #[test]
    fn authenticate_local_app_token_accepts_only_the_exact_bound_app_id() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.tokens = vec![local_app_stored_token("tok-app1", "lmk-app1", "app-1")];

        let headers = with_bearer(get_request("/"), "lmk-app1").headers;
        assert!(authenticate_local_app_token(&deps, &headers, "app-1").is_ok());

        // The exact same token must be rejected for a different app id —
        // this is the core "impossible to do anything beyond running that
        // one recipe" guarantee.
        let rejection = authenticate_local_app_token(&deps, &headers, "app-2");
        assert!(rejection.is_err());
    }

    #[test]
    fn authenticate_local_app_token_rejects_a_token_with_no_local_app_run_scope() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.tokens = vec![stored_token("tok-chat", "lmk-chat-only", vec![Scope::Chat], vec![Backend::Local])];
        let headers = with_bearer(get_request("/"), "lmk-chat-only").headers;
        assert!(authenticate_local_app_token(&deps, &headers, "app-1").is_err());
    }

    #[test]
    fn authenticate_local_app_token_rejects_missing_or_wrong_bearer_and_ignores_require_token_toggle() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = false; // must not matter for this route
        deps.tokens = vec![local_app_stored_token("tok-app1", "lmk-app1", "app-1")];

        let no_bearer = HeaderMap::new();
        assert!(authenticate_local_app_token(&deps, &no_bearer, "app-1").is_err());

        let wrong_bearer = with_bearer(get_request("/"), "lmk-never-existed").headers;
        assert!(authenticate_local_app_token(&deps, &wrong_bearer, "app-1").is_err());
    }

    #[test]
    fn a_local_app_token_cannot_reach_chat_models_or_embeddings_through_the_ordinary_routes() {
        // Exercises the real dispatcher, not just the scope-membership check:
        // a token minted with only `Scope::LocalAppRun` and empty `backends`
        // must be turned away by every one of `handle_request`'s five
        // ordinary routes — this is what makes it structurally impossible
        // for a published Local App's token to do anything beyond running
        // its one bound recipe.
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![local_app_stored_token("tok-app1", "lmk-app1", "app-1")];

        for path in ["/v1/models", "/v1/chat/completions", "/v1/embeddings"] {
            let req = if path == "/v1/models" {
                with_bearer(get_request(path), "lmk-app1")
            } else {
                with_bearer(post_request(path, "{}"), "lmk-app1")
            };
            let (resp, matched) = tokio_test_block_on(handle_request(&deps, req));
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "path {path} must reject a Local-App-scoped token"
            );
            assert_eq!(matched.as_deref(), Some("tok-app1"));
        }
    }

    fn tokio_test_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    // -------------------------------------------------------------
    // Review-finding regressions
    // -------------------------------------------------------------

    #[test]
    fn backend_visible_allows_everything_when_no_token_is_matched() {
        assert!(backend_visible(None, Backend::Local));
        assert!(backend_visible(None, Backend::Ollama));
        assert!(backend_visible(None, Backend::Providers));
    }

    #[test]
    fn backend_visible_respects_a_matched_tokens_backend_list() {
        let auth = TokenAuth {
            id: "t".to_string(),
            scopes: vec![Scope::Models],
            backends: vec![Backend::Local],
            ..Default::default()
        };
        assert!(backend_visible(Some(&auth), Backend::Local));
        assert!(!backend_visible(Some(&auth), Backend::Ollama));
        assert!(!backend_visible(Some(&auth), Backend::Providers));
    }

    /// The core regression for the `handle_models` finding: a token scoped
    /// away from `Backend::Local` must not see the locally-managed model in
    /// the merged listing, even though `scopes` alone (the pre-fix check)
    /// would have let it through.
    #[tokio::test]
    async fn models_endpoint_omits_the_local_model_for_a_token_not_scoped_for_the_local_backend() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-no-local",
            "lmk-no-local",
            vec![Scope::Models],
            vec![Backend::Ollama, Backend::Providers],
        )];

        let req = with_bearer(get_request("/v1/models"), "lmk-no-local");
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(
            data.iter().all(|m| m["owned_by"] != "local"),
            "a token without the `local` backend must never see the locally-managed model"
        );
    }

    /// Positive control for the test above: a token that DOES carry
    /// `Backend::Local` still sees the local model, so the new gate isn't
    /// simply hiding everything.
    #[tokio::test]
    async fn models_endpoint_includes_the_local_model_for_a_token_scoped_for_the_local_backend() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-local",
            "lmk-local",
            vec![Scope::Models],
            vec![Backend::Local],
        )];

        let req = with_bearer(get_request("/v1/models"), "lmk-local");
        let (resp, _) = handle_request(&deps, req).await;
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data
            .iter()
            .any(|m| m["id"] == "qwen2.5-7b-instruct" && m["owned_by"] == "local"));
    }

    /// The core regression for the CORS/`require_token: false` finding: a
    /// browser-style request (one carrying an `Origin` header, exactly what
    /// a same-page or cross-origin `fetch`/`XHR` always attaches) must still
    /// be rejected without a valid bearer token, even when `require_token`
    /// is off — otherwise the server's own wildcard
    /// `Access-Control-Allow-Origin: *` would let any webpage the user has
    /// open drive `/v1/chat/completions` (including a real, credential-
    /// spending provider call) with zero authentication.
    #[tokio::test]
    async fn a_request_carrying_an_origin_header_is_never_exempt_from_auth_even_when_require_token_is_off(
    ) {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = false;
        deps.tokens = vec![stored_token(
            "tok-1",
            "lmk-real-token",
            vec![Scope::Models],
            vec![Backend::Local],
        )];

        let mut req = get_request("/v1/models");
        req.headers
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(matched.is_none());

        // The same request WITH a valid bearer token must still succeed —
        // this isn't a blanket ban on `Origin`, just a requirement that a
        // real token accompany it.
        let mut authed_req = get_request("/v1/models");
        authed_req
            .headers
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let authed_req = with_bearer(authed_req, "lmk-real-token");
        let (resp, matched) = handle_request(&deps, authed_req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(matched.as_deref(), Some("tok-1"));
    }

    /// Regression guard: a request with NO `Origin` header (a normal
    /// non-browser HTTP client — curl, an SDK, an IDE plugin) must keep
    /// working with no token at all when `require_token` is off, exactly
    /// the documented escape hatch for tools that can't set custom headers —
    /// the fix above must not accidentally require a token universally.
    #[tokio::test]
    async fn a_request_with_no_origin_header_still_needs_no_token_when_require_token_is_off() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = false;

        let (resp, matched) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn read_capped_body_returns_the_bytes_when_well_within_the_limit() {
        let stream = futures_util::stream::iter(vec![Ok::<_, BoxError>(Frame::data(
            Bytes::from_static(b"hello world"),
        ))]);
        let bytes = read_capped_body(StreamBody::new(stream), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"hello world");
    }

    #[tokio::test]
    async fn read_capped_body_rejects_a_single_oversized_frame_with_413() {
        let stream = futures_util::stream::iter(vec![Ok::<_, BoxError>(Frame::data(
            Bytes::from_static(b"0123456789"),
        ))]);
        let response = read_capped_body(StreamBody::new(stream), 4)
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = body_bytes(response).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "request_too_large");
    }

    /// The running total, not just any single frame in isolation, must be
    /// checked against the limit — otherwise a caller could smuggle an
    /// arbitrarily large body past the cap by splitting it into many
    /// small-enough frames (exactly what a real chunked-encoded upload from
    /// an oversized client would look like at the hyper frame level).
    #[tokio::test]
    async fn read_capped_body_catches_an_oversized_body_split_across_many_small_frames() {
        let stream = futures_util::stream::iter(vec![
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"12345"))),
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"67890"))),
        ]);
        let response = read_capped_body(StreamBody::new(stream), 6)
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The core regression for the "partial read silently becomes an empty
    /// body" finding: a failed frame read must surface as its own distinct
    /// `400 body_read_error`, never as `Ok(Bytes::new())`.
    #[tokio::test]
    async fn read_capped_body_reports_a_distinct_error_instead_of_silently_substituting_an_empty_body(
    ) {
        let stream = futures_util::stream::iter(vec![Err::<Frame<Bytes>, BoxError>(
            "simulated connection drop".into(),
        )]);
        let response = read_capped_body(StreamBody::new(stream), 1024)
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = body_bytes(response).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "body_read_error");
    }

    /// Pins down the actual fix for the restart-race finding:
    /// `stop_server_core` must `.await` the accept loop's `JoinHandle` (which
    /// only resolves once the task has broken out of its `select!` and
    /// dropped its `TcpListener`) before a caller attempts to rebind the same
    /// port — a synchronous `notify_one()` alone is not enough, since the
    /// spawned task may not even have been polled yet. This mirrors
    /// `run_accept_loop`'s exact `tokio::select!` shape without needing a
    /// real `AppHandle`, run several times to make sure it isn't a fluke.
    #[tokio::test]
    async fn awaiting_the_accept_loops_join_handle_before_rebinding_avoids_the_restart_race() {
        for _ in 0..20 {
            // Bind the accept loop's own listener straight to an OS-assigned
            // port (0) and read the port back off THAT listener, rather than
            // probing with a throwaway `std::net::TcpListener` first and
            // reusing the number after dropping it. The probe-then-reuse
            // shape leaves a window between "probe listener dropped" and
            // "accept-loop listener bound" where any other socket on the
            // machine (including another iteration of this same loop running
            // concurrently under `cargo test`'s default parallelism) can
            // claim that exact port first — a pure test-scaffolding race
            // with zero connection to the actual behavior under test, which
            // this rewrite eliminates entirely by never letting the port go
            // unheld between "chosen" and "in use".
            let listener = bind_listener(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let shutdown = Arc::new(Notify::new());
            let shutdown_for_task = shutdown.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_for_task.notified() => break,
                        _ = listener.accept() => {}
                    }
                }
                // `listener` dropped here, exactly like `run_accept_loop`.
            });

            // Give the spawned task a chance to actually be polled at least
            // once, so it's genuinely sitting inside `select!` — matching
            // production, where the accept loop has been running for a
            // while before any restart is triggered.
            tokio::task::yield_now().await;

            shutdown.notify_one();
            handle.await.unwrap();

            // The rebind itself still has an unavoidable window (closing and
            // reopening a literal port number is inherently a real socket
            // operation, not just an in-process handoff), but it's now the
            // ONLY window in this test, and it's as small as an immediate
            // `.await` — a truly external process would need to win a race
            // measured in microseconds to land in it. A regression in
            // `stop_server_core` itself (the actual thing under test: not
            // awaiting the accept loop's `JoinHandle` before rebinding) would
            // fail this deterministically on every attempt, since the OLD
            // listener would still be alive and holding the port — so a
            // bounded retry here only ever absorbs the residual, unrelated
            // OS-level port race, never a real regression.
            let mut rebound = bind_listener(port).await;
            for _ in 0..2 {
                if rebound.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                rebound = bind_listener(port).await;
            }
            assert!(
                rebound.is_ok(),
                "rebinding after joining the accept loop's task must succeed, not race the old listener's teardown: {rebound:?}"
            );
        }
    }
    /// The admission rule, tested where it actually lives.
    ///
    /// `serve_with_admission` exists as its own function precisely so this is
    /// reachable without 65 sockets and without standing up either accept loop:
    /// the loops differ only in how they build `ServerDeps`, and they must not
    /// differ in whether a request is admitted. That difference is what the bug
    /// was — `run_cli_server` served the identical route set with no permit at
    /// all — so the rule being one function is part of the fix, not a test
    /// convenience.
    mod admission {
        use super::*;
        use crate::http_policy::RequestAdmission;
        use tokio_util::sync::CancellationToken;

        #[tokio::test]
        async fn a_refusal_uses_the_legacy_error_envelope_verbatim() {
            // Byte-asserted rather than status-asserted: an SDK client parses
            // this shape, and the refusal is the one response on this listener
            // that no test covered.
            let admission = RequestAdmission::new(1);
            let shutdown = CancellationToken::new();
            let held = admission.try_admit(&shutdown).expect("first request admits");

            let refused = serve_with_admission(&admission, &shutdown, |_cancel| async {
                panic!("the handler must not run once the quota is exhausted")
            })
            .await;

            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: serde_json::Value =
                serde_json::from_slice(&body_bytes(refused).await).unwrap();
            assert_eq!(
                body,
                json!({
                    "error": {
                        "message": "The API server active-request quota is exhausted",
                        "type": "invalid_request_error",
                        "code": "server_busy",
                    }
                })
            );
            drop(held);
        }

        #[tokio::test]
        async fn the_permit_is_held_until_a_streaming_body_ends_not_until_the_handler_returns() {
            // The defect this closes. A streaming `/v1/chat/completions` returns
            // as soon as upstream headers arrive, so releasing the permit there
            // measured time-to-first-header and bounded nothing: any number of
            // concurrent SSE streams could be in flight against a pool of one.
            let admission = Arc::new(RequestAdmission::new(1));
            let shutdown = CancellationToken::new();
            let (release, wait_for_release) = tokio::sync::oneshot::channel();

            let streaming = serve_with_admission(&admission, &shutdown, |_cancel| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(gated_body(wait_for_release))
                    .unwrap()
            })
            .await;
            assert_eq!(streaming.status(), StatusCode::OK);
            assert_eq!(
                admission.active_requests(),
                1,
                "the handler returned, but its body has not produced a byte — the request is still in flight"
            );

            let refused = serve_with_admission(&admission, &shutdown, |_cancel| async {
                panic!("a second request must not be admitted while the first is still streaming")
            })
            .await;
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);

            release.send(()).unwrap();
            let bytes = body_bytes(streaming).await;
            assert_eq!(&bytes[..], b"data: [DONE]\n\n");
            assert_eq!(
                admission.active_requests(),
                0,
                "the permit must be released when the body ends"
            );

            let admitted_again = serve_with_admission(&admission, &shutdown, |_cancel| async {
                Response::builder().status(StatusCode::OK).body(full_body("ok")).unwrap()
            })
            .await;
            assert_eq!(admitted_again.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn a_client_that_goes_away_mid_stream_releases_its_permit() {
            // Otherwise the pool leaks one permit per abandoned stream and the
            // listener wedges at the quota with nothing running — strictly worse
            // than the unbounded behaviour it replaced.
            let admission = Arc::new(RequestAdmission::new(1));
            let shutdown = CancellationToken::new();
            let (_release, wait_for_release) = tokio::sync::oneshot::channel();

            let streaming = serve_with_admission(&admission, &shutdown, |_cancel| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(gated_body(wait_for_release))
                    .unwrap()
            })
            .await;
            assert_eq!(admission.active_requests(), 1);

            // What hyper does when the connection dies: it drops the body without
            // ever polling it to completion.
            drop(streaming);

            assert_eq!(
                admission.active_requests(),
                0,
                "dropping the response body must release the permit"
            );
        }

        #[tokio::test]
        async fn a_buffered_response_survives_the_wrapper_byte_for_byte() {
            // Every non-streaming route is now wrapped too, so the wrapper must
            // be transparent — including for a body that is already complete.
            let admission = RequestAdmission::new(4);
            let shutdown = CancellationToken::new();

            let response = serve_with_admission(&admission, &shutdown, |_cancel| async {
                json_response(StatusCode::OK, json!({"object": "list", "data": []}))
            })
            .await;

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/json"
            );
            assert_eq!(
                &body_bytes(response).await[..],
                br#"{"data":[],"object":"list"}"#
            );
        }

        /// Structural: both accept loops must reach `serve_one_request` through
        /// the admission helper.
        ///
        /// A behavioural test cannot cover this — it would have to bind a socket
        /// per loop and saturate a pool — and the defect was exactly a *second*
        /// serving path that looked fine in isolation. Asserting on the source is
        /// the cheap guard that a third path cannot be added silently.
        #[test]
        fn no_serving_path_reaches_a_route_without_admission() {
            let source = include_str!("server.rs");
            let production = source
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map(|(before, _)| before)
                .expect("server.rs has a #[cfg(test)] module");

            assert_eq!(
                production.matches("serve_one_request(").count(),
                3,
                "production `serve_one_request` sites changed: one definition plus one \
                 call per accept loop. A new call site must go through \
                 `serve_with_admission`, or that loop bypasses the quota exactly as \
                 `run_cli_server` used to."
            );
            assert!(production.contains("async fn serve_with_admission<Fut>("));
            assert_eq!(
                // The definition is generic, so its name is followed by `<Fut>`
                // and is not counted here — this is calls only, one per loop.
                production.matches("serve_with_admission(").count(),
                2,
                "one call per accept loop"
            );
            assert!(
                !production.contains("drop(guard)"),
                "the guard must be owned by the response body, not dropped when the \
                 handler returns — see `hold_permit_until_body_ends`"
            );
        }
    }
    /// Cancellation, which the guard carried and nothing read.
    ///
    /// The target is **server shutdown**, not client disconnect. A disconnecting
    /// client is already handled by drop — hyper drops the service future and the
    /// reqwest future with it. Stopping the server was the hole:
    /// `stop_server_core` awaits only the accept loop's task, while every
    /// connection is a separate `tokio::spawn` nothing joins, so requests already
    /// accepted kept streaming from upstream after the UI said "stopped".
    mod cancellation {
        use super::*;
        use crate::http_policy::RequestAdmission;
        use tokio_util::sync::CancellationToken;

        /// An upstream that accepts a connection, announces it, and then says
        /// nothing until the test releases it.
        ///
        /// The announcement is the point. A first version cancelled after a fixed
        /// 50ms and passed locally, then failed once with `502` — the fake upstream
        /// had closed its socket first, so `send()` errored before cancellation
        /// won. That is a race, and a loaded CI runner would lose it more often
        /// than a laptop. `connected` makes the ordering explicit: the test cancels
        /// *because* the request is in flight, not after a duration guessed to
        /// coincide with it, and `release` keeps the socket open until the
        /// assertions are done so the upstream can never end the request first.
        struct SilentUpstream {
            base_url: String,
            release: std::sync::mpsc::Sender<()>,
            handle: std::thread::JoinHandle<()>,
        }

        impl SilentUpstream {
            /// Returns the upstream and its "a client connected" signal separately,
            /// so a test can move the signal into a waiter task and still own the
            /// upstream for `finish`.
            fn start() -> (Self, std::sync::mpsc::Receiver<()>) {
                let listener =
                    std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream");
                listener
                    .set_nonblocking(true)
                    .expect("upstream listener goes non-blocking");
                let addr = listener.local_addr().expect("upstream address");
                let (connected_tx, connected) = std::sync::mpsc::channel();
                let (release, release_rx) = std::sync::mpsc::channel();
                // Non-blocking with a deadline rather than a plain `accept()`: one
                // of these tests asserts that *nothing ever connects*, and a
                // blocking accept would never return, so joining would hang the
                // very test it is meant to prove.
                let handle = std::thread::spawn(move || {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(5);
                    let mut accepted = None;
                    while std::time::Instant::now() < deadline {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let _ = connected_tx.send(());
                                accepted = Some(stream);
                                break;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                if release_rx.try_recv().is_ok() {
                                    return;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    }
                    if accepted.is_some() {
                        // Never answered, and never closed early — the request can
                        // only end by being cancelled.
                        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
                    }
                });
                (
                    SilentUpstream {
                        base_url: format!("http://{addr}"),
                        release,
                        handle,
                    },
                    connected,
                )
            }

            fn finish(self) {
                let _ = self.release.send(());
                let _ = self.handle.join();
            }
        }

        #[tokio::test]
        async fn a_stopping_server_answers_an_in_flight_request_instead_of_hanging_on_upstream() {
            let (upstream, connected) = SilentUpstream::start();
            let cancel = CancellationToken::new();
            let deps = test_deps_cancelled_by(upstream.base_url.clone(), cancel.clone());

            // Cancels once the upstream has the connection, so this asserts
            // "cancellation beats an in-flight request" rather than "cancellation
            // beats a 50ms sleep" — which is the race that flaked.
            let cancel_on_connect = cancel.clone();
            let waiter = tokio::task::spawn_blocking(move || {
                let _ = connected.recv_timeout(std::time::Duration::from_secs(5));
                cancel_on_connect.cancel();
            });

            let (resp, _) = handle_request(
                &deps,
                post_request(
                    "/v1/chat/completions",
                    r#"{"model":"llama3.1:8b","stream":false}"#,
                ),
            )
            .await;

            // 503 and not 502: the upstream did nothing wrong, and a client that
            // retries on 502 would retry against a listener that is gone.
            let status = resp.status();
            let body: serde_json::Value =
                serde_json::from_slice(&body_bytes(resp).await).unwrap();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
            assert_eq!(body["error"]["code"], "server_stopping");
            assert_eq!(body["error"]["type"], "invalid_request_error");
            let _ = waiter.await;
            upstream.finish();
        }

        #[tokio::test]
        async fn an_already_cancelled_request_never_reaches_upstream_at_all() {
            // The upstream here would block for its full sleep if contacted, so
            // returning promptly *is* the assertion that no connection was made.
            let (upstream, _connected) = SilentUpstream::start();
            let cancel = CancellationToken::new();
            cancel.cancel();
            let deps = test_deps_cancelled_by(upstream.base_url.clone(), cancel);

            let started = std::time::Instant::now();
            let (resp, _) = handle_request(
                &deps,
                post_request("/v1/embeddings", r#"{"model":"llama3.1:8b","input":"hi"}"#),
            )
            .await;

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                started.elapsed() < std::time::Duration::from_millis(400),
                "a cancelled request waited on upstream anyway: {:?}",
                started.elapsed()
            );
            upstream.finish();
        }

        #[tokio::test]
        async fn a_stream_cut_short_by_shutdown_ends_in_an_error_not_a_clean_close() {
            // The important half. A truncated SSE stream that closes *successfully*
            // is indistinguishable to a client from a complete one that happens to
            // lack `[DONE]` — it would read a partial answer as the whole answer.
            let admission = RequestAdmission::new(2);
            let server_shutdown = CancellationToken::new();
            let (_release, wait_for_release) = tokio::sync::oneshot::channel::<()>();

            let streaming = serve_with_admission(&admission, &server_shutdown, |_cancel| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(gated_body(wait_for_release))
                    .unwrap()
            })
            .await;
            assert_eq!(streaming.status(), StatusCode::OK);

            // What stopping the server does: the accept loop's drop-guard cancels
            // the parent, and every guard's token is a child of it.
            server_shutdown.cancel();

            let collected = streaming.into_body().collect().await;
            let error = collected.err().expect("a cut stream must not collect cleanly");
            assert!(
                error.to_string().contains("stopped while this response was streaming"),
                "unexpected stream error: {error}"
            );
        }

        #[tokio::test]
        async fn a_completed_stream_is_not_reported_as_cut_by_its_own_guard_dropping() {
            // `AdmissionGuard::drop` cancels the token, and the guard lives in the
            // stream's own state — so a naive implementation would race its own
            // teardown and turn every successful stream into an error.
            let admission = RequestAdmission::new(2);
            let server_shutdown = CancellationToken::new();
            let (release, wait_for_release) = tokio::sync::oneshot::channel::<()>();

            let streaming = serve_with_admission(&admission, &server_shutdown, |_cancel| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(gated_body(wait_for_release))
                    .unwrap()
            })
            .await;
            release.send(()).unwrap();

            let bytes = body_bytes(streaming).await;
            assert_eq!(&bytes[..], b"data: [DONE]\n\n");
        }

        #[tokio::test]
        async fn the_token_handed_to_a_handler_is_a_child_of_the_servers() {
            // The link that makes one stop reach every request. Asserted because
            // the guard could be built from a fresh token and every test above
            // would still pass.
            let admission = RequestAdmission::new(2);
            let server_shutdown = CancellationToken::new();
            let seen: Arc<std::sync::Mutex<Option<CancellationToken>>> =
                Arc::new(std::sync::Mutex::new(None));

            let captured = seen.clone();
            let response = serve_with_admission(&admission, &server_shutdown, |cancel| async move {
                *captured.lock().unwrap() = Some(cancel);
                json_response(StatusCode::OK, json!({"ok": true}))
            })
            .await;
            assert_eq!(response.status(), StatusCode::OK);

            let token = seen.lock().unwrap().clone().expect("handler received a token");
            assert!(!token.is_cancelled());
            server_shutdown.cancel();
            assert!(
                token.is_cancelled(),
                "cancelling the server must reach a request's own token"
            );
        }
    }
}
