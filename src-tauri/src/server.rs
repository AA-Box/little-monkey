//! Unified local HTTP server for the OpenAI-compatible and paired-device route
//! families (see `docs/roadmap/p1-local-api-server.md`).
//!
//! This is a *routing reverse proxy and host-service adapter*, not a new
//! inference engine. [`run_unified_endpoint`] is the one production
//! accept/Hyper connection path for both the desktop and `monkey-cli api-serve`;
//! [`UnifiedEndpoint`] supplies the reconciled loopback or LAN/TLS policy. Route
//! authority lives in [`crate::http_route_registry::ROUTES`]. [`EndpointHost`]
//! keeps host-only bookkeeping separate: the desktop supplies an `AppHandle`
//! for [`handle_extended_request`], while the headless CLI hides those routes.
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
//! # K8 scheduler backpressure: nothing to honour here, and why
//!
//! This listener is not a daemon-work producer, so the K8 backpressure signal
//! (`monkey daemon status --json` → `backpressure`) has no refusal to express on
//! any route in this file. That is a consequence of the invariant directly above,
//! not a separate decision: every route either proxies model inference, reads
//! state (`/v1/models`, `/v1/artifacts/{id}`, `/v1/workflows/runs/{id}`,
//! `/v1/knowledge/query`), or — in `handle_local_app_run`'s case — emits
//! `LOCAL_APP_RUN_REQUESTED_EVENT` to the app's own frontend and answers `202`.
//! None of them reach `daemon::enqueue`. The CLI's `api-serve` command invokes
//! this transport, but that does not turn any route into a daemon-work producer.
//!
//! Gating inference on the daemon's job queue would therefore mean spawning
//! `monkey daemon status --json` on the hot path of every chat completion in order
//! to refuse work that never enters the queue being measured. The signal is
//! honoured where work is actually produced: `daemon_commands.rs` (desktop),
//! `monkey-cli`'s `acp.rs` and remote mobile-chat seam, and the CLI itself — all
//! of which funnel through `daemon::enqueue`, which refuses `closed` before
//! creating a worktree or a snapshot.
//!
//! **If a future route does submit daemon work** (the "trigger a workflow over the
//! API" design above is the obvious candidate) it acquires this obligation, and
//! must refuse in the OpenAI-compatible envelope [`error_response`] builds — `429`
//! with `Retry-After` derived from `backpressure.retry_after_ms`, distinct from the
//! `503`/`server_busy` that [`RequestAdmission`] returns. Those two are not the
//! same refusal: admission bounds *requests in flight on this listener* and clears
//! as soon as a response ends, whereas backpressure says the *work queue behind
//! the listener* is full and clears only when runs drain.
//!
//! [`RequestAdmission`]: crate::http_policy::RequestAdmission
//!
//! Structured like `checkpoints.rs`/`web.rs`: an `AppHandle`-free,
//! independently testable core ([`handle_request`]) plus [`EndpointHost`]
//! adapters. The desktop adapter owns `AppState` bookkeeping; the shipped CLI
//! adapter carries its headless runtime into the same listener and connection
//! implementation.
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
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::http_model_catalog::{
    CatalogAuthorization, CatalogBackend, CatalogDispatchTarget, CatalogError, CatalogPolicy,
    CatalogRequestContext, ModelCatalogSource,
};
use crate::http_model_service::{
    openai_model_list, HttpModelService, ModelListRequest, ModelResolveRequest,
};
use crate::http_model_sources::{
    CloudProviderCatalogSource, LegacyLlamaCatalogSource, OllamaCatalogSource,
    OpenAiRuntimeCatalogSource, ProviderCredentialResolver, ProviderCredentialSource,
    StaticLoadedLlamaSnapshot,
};
use crate::http_policy::constant_time_eq;
use crate::http_policy::MAX_REQUEST_BODY_BYTES;
use crate::http_policy::{
    hold_admission_until_response_ends, BoxError, CappedBodyRejection, ResponseBody,
};
use crate::http_route_registry::{
    classify_bearer_family, classify_request, AuthFamily, ClassificationInput, ListenerExposure,
    RouteDecision, RouteFamily, RouteId, RouteOwner,
};
use crate::m3_http_server::{M3HttpModelExtensions, M3HttpRequestService, M3HttpServiceRequest};
use crate::m3_runtime_hub::{M3OperationContext, M3RuntimeHub};
use crate::profiles::ProfileScopedPaths;
use crate::unified_http_server::{
    EndpointTransport, PrimaryServiceConfig, RunningEndpoint, UnifiedEndpoint,
    UnifiedGenerationSpec, UnifiedHttpServerState,
};
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

/// Slow or idle peers consume a connection task before they present a request
/// that can enter [`RequestAdmission`]. Keep that earlier resource bounded as
/// well. The same cap is shared by the desktop endpoints and `api-serve`.
const MAX_HTTP_CONNECTIONS: usize = crate::http_policy::MAX_ACTIVE_REQUESTS * 2;

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

/// Test helper for the primary listener's default busy envelope.
#[cfg(test)]
async fn serve_with_admission<Fut>(
    admission: &crate::http_policy::RequestAdmission,
    server_shutdown: &tokio_util::sync::CancellationToken,
    serve: impl FnOnce(tokio_util::sync::CancellationToken) -> Fut,
) -> Response<ResponseBody>
where
    Fut: std::future::Future<Output = Response<ResponseBody>>,
{
    let refused = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "The API server active-request quota is exhausted",
        "server_busy",
    );
    serve_with_admission_response(admission, server_shutdown, refused, serve).await
}

/// The one implementation of "this listener admits a request".
///
/// The shared desktop/CLI connection path calls it after selecting the route's
/// wire-compatible refusal envelope. It receives the guard's cancellation token,
/// which is how that token reaches `ServerDeps` — a handler cannot be written
/// that silently ignores it, because there is no way to build the deps without
/// one.
async fn serve_with_admission_response<Fut>(
    admission: &crate::http_policy::RequestAdmission,
    server_shutdown: &tokio_util::sync::CancellationToken,
    refused: Response<ResponseBody>,
    serve: impl FnOnce(tokio_util::sync::CancellationToken) -> Fut,
) -> Response<ResponseBody>
where
    Fut: std::future::Future<Output = Response<ResponseBody>>,
{
    let Some(guard) = admission.try_admit(server_shutdown) else {
        return refused;
    };
    let response = serve(guard.cancellation()).await;
    hold_admission_until_response_ends(response, guard)
}

/// Runs one callback after the response body really finishes (or is dropped),
/// preserving the wrapped body's frames and framing metadata exactly.
///
/// This sits *outside* `AdmissionBody`: dropping `inner` first releases the
/// authoritative admission guard and updates its counters, then the callback
/// projects those already-updated counters to the UI. Running the projection
/// when the handler returns would count streaming responses too early.
struct CompletionBody {
    inner: Option<ResponseBody>,
    callback: Option<Box<dyn FnOnce() + Send + Sync + 'static>>,
}

impl CompletionBody {
    fn new(inner: ResponseBody, callback: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            inner: Some(inner),
            callback: Some(Box::new(callback)),
        }
    }

    fn finish(&mut self) {
        // AdmissionBody owns the guard. Its drop must happen before the
        // projection callback reads the counters.
        drop(self.inner.take());
        if let Some(callback) = self.callback.take() {
            callback();
        }
    }
}

impl Drop for CompletionBody {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Body for CompletionBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let poll = match self.inner.as_mut() {
            Some(inner) => Pin::new(inner).poll_frame(cx),
            None => return Poll::Ready(None),
        };
        match poll {
            Poll::Ready(None) => {
                self.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(hyper::body::Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        self.inner
            .as_ref()
            .map(hyper::body::Body::size_hint)
            .unwrap_or_else(|| SizeHint::with_exact(0))
    }
}

fn after_response_ends(
    response: Response<ResponseBody>,
    callback: impl FnOnce() + Send + Sync + 'static,
) -> Response<ResponseBody> {
    response.map(|body| BodyExt::boxed(CompletionBody::new(body, callback)))
}

/// In-memory lifecycle state for the managed API server process — mirrors
/// `llama::LlamaState` field-for-field. No token material lives here as of
/// phase 2 (that was a phase-1-only stopgap before `api_server.json`
/// existed) — tokens are minted/revoked/listed via their own commands below.
pub struct ApiServerState {
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

use crate::http_policy::unix_time_ms as now_ms;

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

/// Finds the stored token whose digest matches `token`, or `None`.
///
/// The one place this file turns a bearer into a [`TokenEntry`]. Both callers —
/// [`authenticate_credential`] and [`authenticate_local_app_token`] — used to
/// carry their own copy of this scan, and a copy is what lets an expiry check or
/// a constant-time compare be fixed in one and not the other.
///
/// **An expired match answers `None`, not the entry**, which is what keeps the
/// two callers' generic 401s honest: neither can distinguish "this token existed
/// but expired" from "this token never existed", because neither is told. What a
/// caller may still do *after* this returns `Some` is refuse for a real reason —
/// possession of a live credential has been proven by then, so a scope or
/// binding denial is safe to name.
///
/// On timing: nothing here compares a raw secret. The incoming bearer is hashed
/// first and only digests are compared, and a digest compare's timing leaks
/// nothing useful about the pre-image — SHA-256's avalanche means learning "the
/// first byte matched" gives an attacker no purchase on which tokens hash to it.
/// [`constant_time_eq`] is used anyway because it is essentially free and keeps
/// "every credential compare in this file is constant-time" true by inspection.
fn find_live_token<'a>(tokens: &'a [StoredToken], token: &str) -> Option<&'a StoredToken> {
    let digest = sha256_hex(token);
    let stored = tokens
        .iter()
        .find(|stored| constant_time_eq(digest.as_bytes(), stored.sha256.as_bytes()))?;
    match stored.expires_at {
        Some(expires_at) if now_ms() >= expires_at => None,
        _ => Some(stored),
    }
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

/// Which upstream classes a [`TokenEntry`] may discover and use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Local,
    Ollama,
    Providers,
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
/// `web.rs::settings_file_path`. `pub` so the shipped `monkey-cli api-serve`
/// subcommand can resolve the same path with its own
/// APP_IDENTIFIER, the same config-drift concern the design doc flags.
pub fn config_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
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
    M3Runtime {
        target: CatalogDispatchTarget,
    },
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
/// decided lazily after authentication by the unified model catalog.
#[derive(Debug, Clone)]
pub struct ProviderSummary {
    pub id: String,
    pub base_url: String,
}

/// Which of the two clients a route's upstream is allowed to be reached with.
///
/// Split out as a pure function of the route so the choice is *assertable*. A
/// `match` inlined at each send site would be equally correct and completely
/// untestable: `reqwest::Client` exposes nothing about its own timeouts or
/// redirect policy, so the only way to prove a route uses the hardened client is
/// to compare which client it picked — which needs the picking to be a function.
///
/// `Unknown` never reaches a send site (it 404s earlier), and returning the local
/// client for it is the safe direction: that client reaches only this machine.
fn client_for<'deps>(deps: &'deps ServerDeps, route: &ModelRoute) -> &'deps reqwest::Client {
    match route {
        // Both endpoint values are trusted loopback runtime configuration;
        // production supplies the app defaults and the CLI harness supplies
        // ephemeral loopback fakes.
        ModelRoute::Llama
        | ModelRoute::Ollama
        | ModelRoute::M3Runtime { .. }
        | ModelRoute::Unknown => &deps.local_client,
        // The provider's `base_url` is whatever the user configured, and the
        // request carries their API key.
        ModelRoute::Providers { .. } => &deps.cloud_client,
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

/// Shared `403` gate for every AppHandle-free or host-only route that requires
/// a specific [`Scope`] — same "`None`
/// auth means unrestricted, `Some` must contain the scope" shape every
/// inline `if let Some(auth) = authed { if !auth.scopes.contains(...) }`
/// check above already uses; factored out here specifically so it's
/// directly unit-testable with no `AppHandle` (see the module doc comment on
/// [`handle_extended_request`]'s host-only routes needing one to actually run,
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

/// Which host-only extended route (if any) a method+path pair matches — pure and
/// `AppHandle`-free so the routing decision itself is unit-testable independently
/// of the AppHandle-requiring handlers it feeds into. Mirrors the plain `match`
/// [`handle_request`] uses for the AppHandle-free routes.
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
    LocalAppStatic {
        app_id: String,
        rel_path: String,
    },
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
    /// not the toggle is on; the catalog policy applies the exposure toggle
    /// before either listing or exact resolution.
    pub providers: Vec<ProviderSummary>,
    tokens: Vec<StoredToken>,
    /// Where this request is recorded in the unified subsystem
    /// event stream (roadmap K12).
    ///
    /// Carried on `ServerDeps` rather than threaded as a parameter because the
    /// two production hosts reach a ledger by different routes — the desktop
    /// host through `AppState`, the CLI host through the data directory it
    /// already derives from `config_path` — and `handle_request` should not have
    /// to know which it is running under. Unit tests build a
    /// [`SubsystemAudit::disabled`] with the reason named, so "no events" in a
    /// test is never mistaken for "this route was never wired up".
    audit: crate::subsystem_audit::SubsystemAudit,
    /// The client for peers on this machine: the bundled `llama-server` and the
    /// local Ollama daemon. Deliberately a default `reqwest::Client`, with no
    /// timeout and reqwest's stock ten-hop redirect policy.
    ///
    /// Kept permissive on purpose rather than by neglect. There is no credential
    /// to forward — neither peer has any authentication at all — and both are
    /// reached at a hardcoded loopback address, so a redirect policy has nothing
    /// to protect. A silence budget would be the actively wrong thing here:
    /// prompt processing on a large context legitimately produces no bytes for
    /// minutes, and this side of the split is where that happens.
    pub local_client: reqwest::Client,
    /// The client for a configured cloud provider, from [`crate::egress::hardened`].
    ///
    /// This is the half that carries a credential to an address the user typed,
    /// so it gets the connect timeout, the silence budget and the redirect policy
    /// that refuses to walk an `x-api-key` to a host a `302` chose. See
    /// `egress::hardened`'s doc for the three holes that closes.
    ///
    /// One client per policy rather than one client per server is the whole point
    /// of this split: the field it replaced served both of these roles, so
    /// hardening it would have applied a silence budget to loopback inference and
    /// leaving it bare left a credential exposed to a redirect. No single policy
    /// fits both, and that is not a defect in either policy.
    pub cloud_client: reqwest::Client,
    /// Release-window limiter for the legacy-token fallback branch. Pairing
    /// tokens use the durable controller limiter; both feed the same unified
    /// route service but are never silently converted into each other.
    pub legacy_rate_limiter: Arc<crate::http_policy::LegacyTokenRateLimiter>,
    model_service: HttpModelService,
    model_extensions: M3HttpModelExtensions,
    m3_service: Option<M3HttpRequestService>,
    m3_policy: Option<Arc<crate::compatibility_hub::LanServerPolicy>>,
    /// This request's cancellation token, from its [`http_policy::AdmissionGuard`].
    ///
    /// A field rather than a parameter so no handler signature has to learn that
    /// cancellation exists — handlers already receive `&ServerDeps`.
    ///
    /// **What this is actually for is server shutdown, not client disconnect.**
    /// A disconnecting client is already handled by drop: hyper drops the service
    /// future, which drops the reqwest future with it. Stopping the API server was
    /// the real hole — the former accept loop spawned connections that nothing
    /// joined, so requests it already accepted kept streaming from upstream after
    /// the user pressed Stop and the UI said "stopped". `stop_running_unified`
    /// now cancels the endpoint token; `run_unified_endpoint` owns the connection
    /// tasks, observes that token, and drains their `JoinSet` before the task
    /// awaited by `stop_server_core` can finish. Every guard's token is a child of
    /// that parent, so honouring this interrupts in-flight upstream work rather
    /// than making the drain wait for it indefinitely.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// A decoded HTTP request. Deliberately not `hyper::Request<Incoming>`
/// itself, so [`handle_request`]'s tests and CLI callers can
/// build one directly without a real hyper connection — [`serve_one_request`]
/// is the thin adapter that buffers a real `Incoming` body into `Bytes` and
/// builds this.
pub struct ServerRequest {
    pub method: Method,
    pub path: String,
    /// The raw query string, without the `?`.
    ///
    /// Carried separately from `path` because every routing decision in this
    /// tree is made on the path alone — `classify_request` and
    /// `DENIED_SURFACES` both match path literals, and folding a query into
    /// that string would let `?x=/v1/agent` near-miss a denial matcher. Only
    /// `/v1/conformance` reads it (K21).
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// One query parameter's raw value, or `None`.
///
/// Hand-parsed rather than pulling in a form decoder: the one caller reads a
/// single integer, and percent-decoding a value nobody percent-encodes would
/// be ceremony.
fn query_param<'query>(query: Option<&'query str>, name: &str) -> Option<&'query str> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

use crate::http_policy::{full_body, json_response};

/// OpenAI-shaped error body: `{"error":{"message","type","code"}}` — the
/// same envelope real OpenAI-compatible clients already parse.
///
/// Deliberately **not** shared with `m3_http_server.rs`'s same-named helper:
/// that one emits `{"error":{"code","message","type":"little_monkey_m3_error"}}`
/// and takes its arguments in the other order. The two envelopes are a
/// client-visible contract (`tests/legacy_route_compatibility.rs` pins these
/// bytes), so merging them would be a wire regression, not a cleanup.
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

/// `GET /v1/conformance` — what this node claims to implement, and the live
/// evidence a conformance run checks the claim against (roadmap K21).
///
/// # Why a read of this route is recorded in the subsystem stream
///
/// `http_action_worth_recording` filters `GET /v1/models` out as discovery,
/// and this is a read too — so the filter's reasoning looks like it should
/// apply. It does not, for two reasons. Asking a node to vouch for itself *is*
/// an act worth an audit row: it is the request that precedes a compatibility
/// claim being made about this machine. And the suite's append-only check
/// needs one action it can perform against any node, including one with no
/// model loaded; this is that action. Recording it is not an oversight, and a
/// future filter line here would silently disable
/// `ledger.append_only`.
///
/// # A ledger that cannot be read is not a node without a ledger
///
/// A read failure answers 503 rather than publishing `ledger: null`. The null
/// means "this listener has no ledger behind it", which a conformance run
/// reports as an honestly-skipped optional section; letting an unreadable
/// database wear that same shape would turn a broken node into a compliant
/// one.
fn handle_conformance_attestation(
    deps: &ServerDeps,
    query: Option<&str>,
) -> Response<ResponseBody> {
    let after =
        query_param(query, crate::conformance::LEDGER_AFTER_PARAM).map_or(Ok(0), str::parse::<u64>);
    let Ok(after) = after else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "'{}' must be a non-negative integer sequence.",
                crate::conformance::LEDGER_AFTER_PARAM
            ),
            "invalid_request_error",
        );
    };

    let ledger = match deps
        .audit
        .chain_evidence(after, crate::conformance::MAX_LEDGER_LINKS)
    {
        Ok(evidence) => evidence,
        Err(error) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("The event ledger could not be read: {error}"),
                "ledger_unreadable",
            )
        }
    };

    let attestation =
        crate::conformance::build_attestation(crate::conformance::AttestationInputs {
            authentication_required: deps.require_token,
            ledger,
        });
    match serde_json::to_value(&attestation) {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("The conformance attestation could not be encoded: {error}"),
            "attestation_encode_failed",
        ),
    }
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
fn authenticate_credential(
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

    // An expired token reaches the same generic error as an unknown one, because
    // `find_live_token` answers `None` for both — see its doc comment.
    let Some(stored) = find_live_token(&deps.tokens, token) else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Incorrect API key provided. Find the current one in Little Monkey's Settings > API Server.",
            "invalid_api_key",
        ));
    };
    Ok(Some(TokenAuth {
        id: stored.id.clone(),
        scopes: stored.scopes.clone(),
        backends: stored.backends.clone(),
        bound_local_app_id: stored.bound_local_app_id.clone(),
    }))
}

fn debit_legacy_auth(
    deps: &ServerDeps,
    authed: Option<&TokenAuth>,
    input_bytes: u64,
) -> Result<(), Response<ResponseBody>> {
    let Some(authed) = authed else {
        return Ok(());
    };
    if let Err(retry_after_ms) =
        deps.legacy_rate_limiter
            .check_and_debit(&authed.id, input_bytes, now_ms())
    {
        let mut response = error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "This API token has exceeded its request or input-byte limit.",
            "rate_limit_exceeded",
        );
        if let Ok(value) = HeaderValue::from_str(&retry_after_ms.div_ceil(1_000).max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return Err(response);
    }
    Ok(())
}

fn authenticate(
    deps: &ServerDeps,
    headers: &HeaderMap,
    input_bytes: u64,
) -> Result<Option<TokenAuth>, Response<ResponseBody>> {
    let authed = authenticate_credential(deps, headers)?;
    debit_legacy_auth(deps, authed.as_ref(), input_bytes)?;
    Ok(authed)
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

fn legacy_catalog_authorization(authed: Option<&TokenAuth>) -> CatalogAuthorization {
    let mut allowed_backends = std::collections::BTreeSet::new();
    if backend_visible(authed, Backend::Local) {
        allowed_backends.insert(CatalogBackend::ManagedLocal);
        allowed_backends.insert(CatalogBackend::Mlx);
    }
    if backend_visible(authed, Backend::Ollama) {
        allowed_backends.insert(CatalogBackend::Ollama);
    }
    if backend_visible(authed, Backend::Providers) {
        allowed_backends.insert(CatalogBackend::CloudProvider);
    }
    CatalogAuthorization::Authorized { allowed_backends }
}

fn legacy_catalog_policy(deps: &ServerDeps) -> CatalogPolicy {
    let mut enabled_backends =
        std::collections::BTreeSet::from([CatalogBackend::ManagedLocal, CatalogBackend::Mlx]);
    if deps.expose_ollama {
        enabled_backends.insert(CatalogBackend::Ollama);
    }
    if deps.expose_providers {
        enabled_backends.insert(CatalogBackend::CloudProvider);
    }
    CatalogPolicy { enabled_backends }
}

fn legacy_catalog_context(deps: &ServerDeps) -> CatalogRequestContext {
    CatalogRequestContext::with_timeout(
        deps.cancel.clone(),
        std::time::Duration::from_secs(30 * 60),
    )
}

fn legacy_catalog_error(error: CatalogError) -> Response<ResponseBody> {
    if matches!(error, CatalogError::Cancelled) {
        return cancelled_response();
    }
    let (status, code) = match &error {
        CatalogError::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid_api_key"),
        CatalogError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        CatalogError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
        CatalogError::Cancelled => unreachable!("handled above"),
        CatalogError::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
        CatalogError::InvalidRequest(_) | CatalogError::InvalidSource(_) => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        CatalogError::LimitExceeded { .. } => {
            (StatusCode::PAYLOAD_TOO_LARGE, "catalog_limit_exceeded")
        }
        CatalogError::SourceUnavailable { .. } => (StatusCode::BAD_GATEWAY, "upstream_unreachable"),
        CatalogError::NotFound { .. } => (StatusCode::NOT_FOUND, "model_not_found"),
        CatalogError::Conflict(_) => (StatusCode::CONFLICT, "model_conflict"),
    };
    let mut response = error_response(status, &error.to_string(), code);
    if let CatalogError::RateLimited { retry_after_ms } = error {
        if let Ok(value) = HeaderValue::from_str(&retry_after_ms.div_ceil(1_000).max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

async fn resolve_legacy_model(
    deps: &ServerDeps,
    authed: Option<&TokenAuth>,
    model_id: &str,
    runtime_override: Option<&str>,
) -> Result<ModelRoute, Response<ResponseBody>> {
    let policy = legacy_catalog_policy(deps);
    let context = legacy_catalog_context(deps);
    let model_result = deps
        .model_service
        .resolve(ModelResolveRequest {
            authorization: legacy_catalog_authorization(authed),
            policy: &policy,
            allowed_models: &std::collections::BTreeSet::new(),
            model_id,
            runtime_override,
            extra_sources: &deps.model_extensions.sources,
            context: &context,
        })
        .await;
    let model = match model_result {
        Ok(model) => model,
        Err(CatalogError::SourceUnavailable {
            failure: crate::http_model_catalog::CatalogSourceError::PermissionDenied,
            ..
        }) if model_id.contains('/') => {
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                "No API key is configured for this provider.",
                "provider_not_configured",
            ));
        }
        Err(error) => return Err(legacy_catalog_error(error)),
    };
    match model.into_dispatch_target().map_err(legacy_catalog_error)? {
        CatalogDispatchTarget::Provider {
            provider_id,
            provider_model_id,
            ..
        } => Ok(ModelRoute::Providers {
            provider_id,
            model_id: provider_model_id,
        }),
        CatalogDispatchTarget::Runtime {
            runtime_id,
            backend: CatalogBackend::ManagedLocal,
            ..
        } if runtime_id == "managed-llama" => Ok(ModelRoute::Llama),
        CatalogDispatchTarget::Runtime {
            runtime_id,
            backend: CatalogBackend::Ollama,
            ..
        } if runtime_id == "ollama" => Ok(ModelRoute::Ollama),
        target @ CatalogDispatchTarget::Runtime { .. } => Ok(ModelRoute::M3Runtime { target }),
    }
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

    let policy = legacy_catalog_policy(deps);
    let context = legacy_catalog_context(deps);
    let allowed_models = std::collections::BTreeSet::new();
    match deps
        .model_service
        .list(ModelListRequest {
            authorization: legacy_catalog_authorization(authed),
            policy: &policy,
            allowed_models: &allowed_models,
            extra_sources: &deps.model_extensions.sources,
            context: &context,
        })
        .await
    {
        Ok(mut models) => {
            for model in &mut models {
                if model.backend == CatalogBackend::ManagedLocal {
                    model.owned_by = "local".to_string();
                }
            }
            json_response(StatusCode::OK, openai_model_list(&models, false))
        }
        Err(error) => legacy_catalog_error(error),
    }
}

async fn handle_chat_completions(
    deps: &ServerDeps,
    authed: Option<&TokenAuth>,
    headers: &HeaderMap,
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

    if model.trim().is_empty() {
        // Mirrors OpenAI's own wording for a request with no `model`.
        return error_response(
            StatusCode::NOT_FOUND,
            "you must provide a model parameter",
            "model_not_found",
        );
    }

    if let Some((provider_id, _)) = model.split_once('/') {
        if deps
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
            && !deps.expose_providers
        {
            // Preserve the legacy surface's "unexposed means unknown" byte
            // contract. The shared catalog deliberately reports a policy
            // denial for paired callers, so this compatibility translation
            // must happen at the legacy edge before any source is polled.
            return error_response(
                StatusCode::NOT_FOUND,
                &format!("Unknown model '{model}'"),
                "model_not_found",
            );
        }
    }

    let runtime_override = headers
        .get("x-little-monkey-runtime-id")
        .and_then(|value| value.to_str().ok());
    let route = match resolve_legacy_model(deps, authed, &model, runtime_override).await {
        Ok(route) => route,
        Err(response) => return response,
    };

    if let ModelRoute::M3Runtime { target } = &route {
        let Some(service) = &deps.m3_service else {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "The resolved M3 runtime is unavailable",
                "upstream_unreachable",
            );
        };
        return service
            .clone()
            .with_model_extensions(deps.model_extensions.clone())
            .dispatch_resolved_internal_runtime(
                RouteId::ChatCompletions,
                target.clone(),
                body,
                M3OperationContext {
                    cancellation: deps.cancel.clone(),
                    timeout_ms: 30 * 60 * 1_000,
                },
            )
            .await;
    }

    let request_builder = match &route {
        ModelRoute::Llama => client_for(deps, &route)
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                deps.llama_port
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Ollama => client_for(deps, &route)
            .post(format!("{}/v1/chat/completions", deps.ollama_base_url))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Providers {
            provider_id,
            model_id,
        } => {
            // `provider_id` is guaranteed to match an entry in
            // `deps.providers` — the catalog only produces this variant for
            // a configured provider id — but a defensive `NOT_FOUND`
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
            let request = client_for(deps, &route)
                .post(format!("{base_url}/chat/completions"))
                .bearer_auth(&api_key)
                .json(&outgoing);
            providers::add_anthropic_headers(request, provider_id, &api_key)
        }
        ModelRoute::M3Runtime { .. } => unreachable!("handled above"),
        ModelRoute::Unknown => unreachable!("handled above"),
    };

    // Metered, and safe on the streaming branch below: `egress::send` wraps the
    // body in a frame-by-frame passthrough that adds a counter to `poll_frame` and
    // forwards `size_hint`/`is_end_stream` untouched, so it buffers nothing. That
    // matters here specifically because this one call site serves both the
    // buffered and the raw-SSE response, and buffering an SSE stream is a hang
    // rather than a delay. `streamed_upstream_sse_bytes_reach_the_client_unmodified`
    // pins the bytes.
    let Some(sent) = unless_cancelled(&deps.cancel, crate::egress::send(request_builder)).await
    else {
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
    headers: &HeaderMap,
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

    if model.trim().is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "you must provide a model parameter",
            "model_not_found",
        );
    }

    if let Some((provider_id, _)) = model.split_once('/') {
        if deps
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
        {
            if !deps.expose_providers {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!("Unknown model '{model}'"),
                    "model_not_found",
                );
            }
            if !backend_visible(authed, Backend::Providers) {
                return forbidden_response("This token isn't scoped for the 'providers' backend.");
            }
            return error_response(
                StatusCode::NOT_IMPLEMENTED,
                "Embeddings via a cloud provider aren't supported yet — use a local llama-server model (started with --embeddings) or an Ollama tag.",
                "embeddings_not_supported",
            );
        }
    }

    let runtime_override = headers
        .get("x-little-monkey-runtime-id")
        .and_then(|value| value.to_str().ok());
    let route = match resolve_legacy_model(deps, authed, &model, runtime_override).await {
        Ok(route) => route,
        Err(response) => return response,
    };

    if let ModelRoute::Providers { .. } = &route {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Embeddings via a cloud provider aren't supported yet — use a local llama-server model (started with --embeddings) or an Ollama tag.",
            "embeddings_not_supported",
        );
    }

    if route == ModelRoute::Llama && !deps.llama_embeddings_enabled {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "This model wasn't started with embeddings support. Restart it with the \"Start with embeddings support\" option checked in the Models panel.",
            "embeddings_not_enabled",
        );
    }

    if let ModelRoute::M3Runtime { target } = &route {
        let Some(service) = &deps.m3_service else {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "The resolved M3 runtime is unavailable",
                "upstream_unreachable",
            );
        };
        return service
            .clone()
            .with_model_extensions(deps.model_extensions.clone())
            .dispatch_resolved_internal_runtime(
                RouteId::Embeddings,
                target.clone(),
                body,
                M3OperationContext {
                    cancellation: deps.cancel.clone(),
                    timeout_ms: 30 * 60 * 1_000,
                },
            )
            .await;
    }

    let upstream_url = match route {
        ModelRoute::Llama => format!("http://127.0.0.1:{}/v1/embeddings", deps.llama_port),
        ModelRoute::Ollama => format!("{}/v1/embeddings", deps.ollama_base_url),
        ModelRoute::Providers { .. } | ModelRoute::M3Runtime { .. } | ModelRoute::Unknown => {
            unreachable!("handled above")
        }
    };

    let request = client_for(deps, &route)
        .post(&upstream_url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    let Some(sent) = unless_cancelled(&deps.cancel, crate::egress::send(request)).await else {
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
    let Some(stored) = find_live_token(&deps.tokens, token) else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Incorrect API key provided for this Local App.",
            "invalid_api_key",
        ));
    };
    // Safe to name a real reason: the digest matched an unexpired token, so the
    // caller has already proven possession of a live credential.
    if !stored.scopes.contains(&Scope::LocalAppRun)
        || stored.bound_local_app_id.as_deref() != Some(app_id)
    {
        return Err(forbidden_response(
            "This token isn't scoped to run this Local App.",
        ));
    }
    Ok(TokenAuth {
        id: stored.id.clone(),
        scopes: stored.scopes.clone(),
        backends: stored.backends.clone(),
        bound_local_app_id: stored.bound_local_app_id.clone(),
    })
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
    let Ok(app_data_dir) = app.profile_data_dir() else {
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
            .expect(
                "building a static-file response from a fixed status + content-type never fails",
            ),
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
    if !crate::local_apps::is_valid_app_id(&app_id)
        || authed.bound_local_app_id.as_deref() != Some(app_id.as_str())
    {
        return not_found_response();
    }
    let Ok(app_data_dir) = app.profile_data_dir() else {
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
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e, "internal_error"),
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

/// Dispatches the host-only extended routes when `path`/`method` match
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
        if let Err(retry_after_ms) =
            deps.legacy_rate_limiter
                .check_and_debit(&authed.id, body.len() as u64, now_ms())
        {
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "This Local App token has exceeded its request or input-byte limit.",
                "rate_limit_exceeded",
            );
            if let Ok(value) =
                HeaderValue::from_str(&retry_after_ms.div_ceil(1_000).max(1).to_string())
            {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return Some((with_cors(response), None));
        }
        let matched_token_id = Some(authed.id.clone());
        let response = handle_local_app_run(app, &authed, app_id.clone(), body.clone()).await;
        return Some((with_cors(response), matched_token_id));
    }

    let authed = match authenticate(deps, headers, body.len() as u64) {
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
        query,
        headers,
        body,
    } = req;

    // `/health` is the one unauthenticated route (a liveness probe has to
    // work before a caller has a token to hand it).
    if method == Method::GET && path == "/health" {
        return (with_cors(health_response()), None);
    }

    // `GET /v1/contract` is the second one, and for the same reason (roadmap
    // K19): a caller asks which ABI this instance implements before it can
    // know whether the credential shape it holds is still the right one. The
    // body is a pure function of the built binary — no configuration, no
    // model list, no token state.
    if method == Method::GET && path == "/v1/contract" {
        return (
            with_cors(json_response(
                StatusCode::OK,
                crate::contract::introspection(),
            )),
            None,
        );
    }

    // CORS preflight never carries a bearer token — browsers deliberately
    // don't attach one to an `OPTIONS` request.
    if method == Method::OPTIONS && path.starts_with("/v1/") {
        return (cors_preflight_response(), None);
    }

    let authed = match authenticate(deps, &headers, body.len() as u64) {
        Ok(authed) => authed,
        Err(response) => return (with_cors(response), None),
    };
    let matched_token_id = authed.as_ref().map(|a| a.id.clone());

    let response = match (&method, path.as_str()) {
        (&Method::GET, crate::conformance::ATTESTATION_PATH) => {
            handle_conformance_attestation(deps, query.as_deref())
        }
        (&Method::GET, "/v1/models") => handle_models(deps, authed.as_ref()).await,
        (&Method::POST, "/v1/chat/completions") => {
            handle_chat_completions(deps, authed.as_ref(), &headers, body).await
        }
        (&Method::POST, "/v1/embeddings") => {
            handle_embeddings(deps, authed.as_ref(), &headers, body).await
        }
        _ => not_found_response(),
    };

    record_http_request(deps, &method, &path, response.status());

    (with_cors(response), matched_token_id)
}

/// Record one served HTTP request in the unified subsystem event stream
/// (roadmap K12).
///
/// **Which requests are recorded, and why not all of them.** An inbound request
/// that runs a model, reads the user's knowledge base or returns an artifact is
/// an action taken on this machine at someone else's request, and that is what
/// the stream is for. `/health`, a CORS preflight and `GET /v1/models` are not:
/// they are the discovery calls every client makes *before* acting, they carry
/// no effect, and recording them would double the stream while making the rows
/// that matter harder to find. They are filtered by
/// [`http_action_worth_recording`] rather than here so the rule is one testable
/// function instead of a condition buried in a handler.
///
/// The response *status* is recorded, never the body or the headers: a body may
/// hold the user's own text and `detail_json` is covered by the chain, so it is
/// permanent. An authorization header would be worse still.
fn record_http_request(deps: &ServerDeps, method: &Method, path: &str, status: StatusCode) {
    let Some(action) = http_action_worth_recording(method, path) else {
        return;
    };
    deps.audit.record(crate::subsystem_audit::SubsystemAction {
        subsystem: crate::run_ledger::Subsystem::Http,
        action,
        // An inbound request is not a run and never will be — `run_scope`'s
        // `InboundRequest` exists to say exactly that — so no turn is passed and
        // the attribution comes from the ambient scope.
        turn_id: None,
        // Bearer-token auth, not the permission gate: nothing here goes through
        // `request_permission`, so there is no decision to point at. `None` is
        // the honest answer and the CLI prints it as "nothing gated this
        // action" rather than leaving it blank.
        permission_request_id: None,
        outcome: http_outcome(status),
        detail: Some(serde_json::json!({ "status": status.as_u16() })),
    });
}

/// How a response status reads as an outcome.
///
/// `Denied` is kept apart from `Failed` for the reason `SubsystemOutcome` gives:
/// a refusal and an error are different findings, and a reader counting failures
/// must not be counting refusals. A pure function so the mapping is testable —
/// asserting it inline in the caller would only restate it.
fn http_outcome(status: StatusCode) -> crate::run_ledger::SubsystemOutcome {
    crate::subsystem_audit::outcome_for_status(status.as_u16())
}

/// The action string for a request worth recording, or `None` to skip it.
///
/// Kept pure and separate from [`record_http_request`] so the rule can be tested
/// without standing up a listener — and so adding a route means adding a line
/// here rather than remembering to instrument a handler.
fn http_action_worth_recording(method: &Method, path: &str) -> Option<String> {
    // A preflight carries no bearer token and takes no action; a liveness probe
    // is answered before authentication even runs.
    if method == Method::OPTIONS || path == "/health" {
        return None;
    }
    // Discovery, not action: every client asks before it does anything, and the
    // request that follows is the one that acted. `/v1/contract` is the same
    // kind of question one step earlier — which ABI is this? — answered from a
    // constant, so it is filtered here for the same reason.
    if method == Method::GET && (path == "/v1/models" || path == "/v1/contract") {
        return None;
    }
    Some(format!("{method} {path}"))
}

// ---------------------------------------------------------------------
// unified Hyper transport and host-specific request state
// ---------------------------------------------------------------------

/// The desktop parts of [`ServerDeps`] that stay fixed for one reconciled
/// generation — built by [`runtime_for_spec`], cloned and policy-specialized for
/// each prepared endpoint, then shared through [`EndpointHost`]. [`build_deps`]
/// combines them with the live llama status and token list for each request.
#[derive(Clone)]
struct ServerRuntime {
    /// See [`ServerDeps::local_client`] — built once per server, cloned per request.
    local_client: reqwest::Client,
    /// See [`ServerDeps::cloud_client`].
    cloud_client: reqwest::Client,
    ollama_base_url: String,
    require_token: bool,
    expose_ollama: bool,
    expose_providers: bool,
    legacy_rate_limiter: Arc<crate::http_policy::LegacyTokenRateLimiter>,
    model_service: HttpModelService,
    m3_service: Option<M3HttpRequestService>,
    m3_policy: Option<Arc<crate::compatibility_hub::LanServerPolicy>>,
}

type CustomProviderLoader = dyn Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static;

struct CliServerRuntime {
    config_path: PathBuf,
    load_custom_providers: Arc<CustomProviderLoader>,
    state: Arc<AppState>,
    local_client: reqwest::Client,
    cloud_client: reqwest::Client,
    llama_port: u16,
    ollama_base_url: String,
    legacy_rate_limiter: Arc<crate::http_policy::LegacyTokenRateLimiter>,
    model_service: HttpModelService,
    m3_service: M3HttpRequestService,
    m3_policy: Option<Arc<crate::compatibility_hub::LanServerPolicy>>,
    audit: crate::subsystem_audit::SubsystemAudit,
}

#[derive(Clone)]
enum EndpointHost {
    Desktop {
        app: AppHandle,
        runtime: Arc<ServerRuntime>,
    },
    Cli(Arc<CliServerRuntime>),
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

enum LegacyLlamaInventory {
    Snapshot(Option<String>),
    OpenAiModels(reqwest::Url),
}

fn model_extensions(
    llama_inventory: LegacyLlamaInventory,
    expose_ollama: bool,
    expose_providers: bool,
    ollama_base_url: &str,
    providers_catalog: &[ProviderSummary],
    local_client: &reqwest::Client,
    cloud_client: &reqwest::Client,
) -> M3HttpModelExtensions {
    let managed_source: Arc<dyn ModelCatalogSource> = match llama_inventory {
        LegacyLlamaInventory::Snapshot(model_id) => Arc::new(LegacyLlamaCatalogSource::new(
            Arc::new(StaticLoadedLlamaSnapshot { model_id }),
            "legacy-managed-llama",
            "managed-llama",
        )),
        LegacyLlamaInventory::OpenAiModels(models_url) => {
            Arc::new(OpenAiRuntimeCatalogSource::new(
                "legacy-managed-llama",
                "managed-llama",
                CatalogBackend::ManagedLocal,
                "local",
                models_url,
                local_client.clone(),
            ))
        }
    };
    let mut sources: Vec<Arc<dyn ModelCatalogSource>> = vec![managed_source];
    if expose_ollama {
        if let Ok(base_url) = reqwest::Url::parse(ollama_base_url) {
            if let Ok(source) =
                OllamaCatalogSource::new("legacy-ollama", "ollama", base_url, local_client.clone())
            {
                sources.push(Arc::new(source));
            }
        }
    }
    let credentials: Arc<dyn ProviderCredentialSource> =
        Arc::new(ProviderCredentialResolver::new(|provider_id| {
            providers::read_key(provider_id)
                .map(Some)
                .map_err(|_| crate::http_model_catalog::CatalogSourceError::PermissionDenied)
        }));
    let provider_extensions = provider_model_extensions(
        expose_providers,
        providers_catalog,
        cloud_client,
        credentials,
    );
    sources.extend(provider_extensions.sources);
    M3HttpModelExtensions {
        sources,
        provider_base_urls: provider_extensions.provider_base_urls,
        cloud_client: provider_extensions.cloud_client,
        provider_credentials: provider_extensions.provider_credentials,
    }
}

pub(crate) fn provider_model_extensions(
    enabled: bool,
    providers_catalog: &[ProviderSummary],
    cloud_client: &reqwest::Client,
    credentials: Arc<dyn ProviderCredentialSource>,
) -> M3HttpModelExtensions {
    let mut sources: Vec<Arc<dyn ModelCatalogSource>> = Vec::new();
    let mut provider_base_urls = std::collections::BTreeMap::new();
    if enabled {
        for provider in providers_catalog {
            let Ok(base_url) = reqwest::Url::parse(&provider.base_url) else {
                continue;
            };
            let Ok(source) = CloudProviderCatalogSource::new(
                provider.id.clone(),
                base_url,
                cloud_client.clone(),
                credentials.clone(),
            ) else {
                continue;
            };
            provider_base_urls.insert(provider.id.clone(), provider.base_url.clone());
            sources.push(Arc::new(source));
        }
    }
    M3HttpModelExtensions {
        sources,
        provider_base_urls,
        cloud_client: enabled.then(|| cloud_client.clone()),
        provider_credentials: enabled.then_some(credentials),
    }
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

fn primary_m3_policy(
    configured: Option<&crate::compatibility_hub::LanServerPolicy>,
    port: u16,
) -> Option<Arc<crate::compatibility_hub::LanServerPolicy>> {
    let mut policy = configured?.clone();
    policy.bind_address = "127.0.0.1".to_string();
    policy.port = port;
    policy.tls = crate::compatibility_hub::TlsPolicy::Disabled;
    // A network request is never promoted to `HttpAuth::Internal`. The
    // primary socket accepts M3-only routes only through a real persisted
    // pairing policy and a scoped bearer token.
    policy.require_authentication = true;
    policy.pairing_required = true;
    Some(Arc::new(policy))
}

pub(crate) fn provider_sources_enabled(
    legacy_expose_providers: bool,
    policy: Option<&crate::compatibility_hub::LanServerPolicy>,
) -> bool {
    legacy_expose_providers
        || policy.is_some_and(|policy| {
            policy
                .allowed_backends
                .contains(&crate::compatibility_hub::ApiBackend::CloudProvider)
        })
}

fn bounded_loopback_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(5))
        .read_timeout(std::time::Duration::from_secs(30 * 60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Failed to build the bounded loopback HTTP client: {error}"))
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
    let providers = build_provider_catalog(app);
    let model_extensions = model_extensions(
        LegacyLlamaInventory::Snapshot(llama_ready.then(|| llama_model_stem.clone()).flatten()),
        runtime.expose_ollama,
        provider_sources_enabled(runtime.expose_providers, runtime.m3_policy.as_deref()),
        &runtime.ollama_base_url,
        &providers,
        &runtime.local_client,
        &runtime.cloud_client,
    );

    ServerDeps {
        llama_port,
        llama_ready,
        llama_model_stem,
        llama_embeddings_enabled,
        ollama_base_url: runtime.ollama_base_url.clone(),
        require_token: runtime.require_token,
        expose_ollama: runtime.expose_ollama,
        expose_providers: runtime.expose_providers,
        providers,
        tokens,
        local_client: runtime.local_client.clone(),
        cloud_client: runtime.cloud_client.clone(),
        legacy_rate_limiter: runtime.legacy_rate_limiter.clone(),
        model_service: runtime.model_service.clone(),
        model_extensions,
        m3_service: runtime.m3_service.clone(),
        m3_policy: runtime.m3_policy.clone(),
        cancel,
        audit: crate::subsystem_audit::SubsystemAudit::desktop(app.clone()),
    }
}

impl EndpointHost {
    fn m3_policy(&self) -> Option<Arc<crate::compatibility_hub::LanServerPolicy>> {
        match self {
            Self::Desktop { runtime, .. } => runtime.m3_policy.clone(),
            Self::Cli(runtime) => runtime.m3_policy.clone(),
        }
    }

    fn app_handle(&self) -> Option<&AppHandle> {
        match self {
            Self::Desktop { app, .. } => Some(app),
            Self::Cli(_) => None,
        }
    }

    fn build_deps(&self, cancel: tokio_util::sync::CancellationToken) -> ServerDeps {
        match self {
            Self::Desktop { app, runtime } => build_deps(app, runtime, cancel),
            Self::Cli(runtime) => {
                let config = load_config_impl(&runtime.config_path).unwrap_or_default();
                let providers = provider_catalog_from((runtime.load_custom_providers)());
                let llama_models_url = reqwest::Url::parse(&format!(
                    "http://127.0.0.1:{}/v1/models",
                    runtime.llama_port
                ))
                .expect("loopback llama model URL is valid");
                let model_extensions = model_extensions(
                    LegacyLlamaInventory::OpenAiModels(llama_models_url),
                    config.expose_ollama,
                    provider_sources_enabled(config.expose_providers, runtime.m3_policy.as_deref()),
                    &runtime.ollama_base_url,
                    &providers,
                    &runtime.local_client,
                    &runtime.cloud_client,
                );
                ServerDeps {
                    audit: runtime.audit.clone(),
                    llama_port: runtime.llama_port,
                    llama_ready: false,
                    llama_model_stem: None,
                    // The CLI cannot observe the GUI-only llama embeddings flag.
                    llama_embeddings_enabled: false,
                    ollama_base_url: runtime.ollama_base_url.clone(),
                    require_token: config.require_token,
                    expose_ollama: config.expose_ollama,
                    expose_providers: config.expose_providers,
                    providers,
                    tokens: tokens_from_config(&config),
                    local_client: runtime.local_client.clone(),
                    cloud_client: runtime.cloud_client.clone(),
                    legacy_rate_limiter: runtime.legacy_rate_limiter.clone(),
                    model_service: runtime.model_service.clone(),
                    model_extensions,
                    m3_service: Some(runtime.m3_service.clone()),
                    m3_policy: runtime.m3_policy.clone(),
                    cancel,
                }
            }
        }
    }

    async fn serve_request(
        &self,
        req: Request<Incoming>,
        endpoint: &UnifiedEndpoint,
        remote_address: IpAddr,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Response<ResponseBody> {
        let deps = self.build_deps(cancel);
        let (response, matched_token_id) = match serve_one_request(
            deps,
            req,
            self.app_handle(),
            endpoint.exposure,
            endpoint.primary,
            remote_address,
        )
        .await
        {
            Ok(value) => value,
            Err(never) => match never {},
        };
        match self {
            Self::Desktop { app, .. } => {
                if let Some(token_id) = matched_token_id {
                    record_token_used(app, &token_id);
                }
            }
            Self::Cli(runtime) => {
                if let Some(token_id) = &matched_token_id {
                    record_token_used_with_state(&runtime.state, &runtime.config_path, token_id);
                }
                eprintln!(
                    "[api-serve] request {} -> {}",
                    req_log_hint(matched_token_id.as_deref()),
                    response.status()
                );
            }
        }
        response
    }

    fn after_response_ends(&self, response: Response<ResponseBody>) -> Response<ResponseBody> {
        match self {
            Self::Desktop { app, .. } => {
                let app = app.clone();
                after_response_ends(response, move || {
                    let _ = sync_api_server_projection(&app);
                })
            }
            Self::Cli(_) => response,
        }
    }

    fn record_hidden_response(&self, status: StatusCode) {
        if matches!(self, Self::Cli(_)) {
            eprintln!("[api-serve] request {} -> {status}", req_log_hint(None));
        }
    }
}

/// The shared capped body read ([`crate::http_policy::read_capped_body`]) in the
/// legacy route family's own wire bytes.
///
/// The read *semantics* — never buffering past the cap, ignoring
/// `Content-Length`, dropping rather than draining a rejected body — live in
/// `http_policy.rs` so a change to them cannot take effect on one route family
/// and silently not on the other. Only the rendering is local, and it has to be:
/// the legacy family owes an OpenAI-shaped envelope plus the wildcard CORS header
/// that `tests/legacy_route_compatibility.rs` pins, where the compatibility
/// router owes `little_monkey_m3_error` under its own origin allowlist.
async fn read_capped_body<B>(
    body: B,
    limit: usize,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Bytes, Response<ResponseBody>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    crate::http_policy::read_capped_body(body, limit, cancellation)
        .await
        .map_err(|rejection| match rejection {
            CappedBodyRejection::Cancelled => with_cors(cancelled_response()),
            CappedBodyRejection::TooLarge { limit } => with_cors(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("Request body exceeds the {limit}-byte limit."),
                "request_too_large",
            )),
            CappedBodyRejection::ReadFailed => with_cors(error_response(
                StatusCode::BAD_REQUEST,
                "The request body could not be fully read — the connection was interrupted or the transfer encoding was malformed.",
                "body_read_error",
            )),
        })
}

async fn dispatch_m3_route(
    deps: &ServerDeps,
    route: RouteId,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    remote_address: IpAddr,
) -> Response<ResponseBody> {
    let (Some(service), Some(policy)) = (&deps.m3_service, &deps.m3_policy) else {
        return with_cors(not_found_response());
    };
    service
        .clone()
        .with_model_extensions(deps.model_extensions.clone())
        .handle(M3HttpServiceRequest {
            route,
            method,
            headers,
            body,
            remote_address,
            policy: policy.clone(),
            context: M3OperationContext {
                cancellation: deps.cancel.clone(),
                timeout_ms: 30 * 60 * 1_000,
            },
        })
        .await
}

/// Adapts a real hyper request into the `AppHandle`-free [`handle_request`]
/// core: reads the body (capped — see [`read_capped_body`]) and hands
/// everything off unchanged. `app`, when supplied by the desktop host, also
/// gives [`handle_extended_request`] a chance to claim a host-only route before
/// falling through to the AppHandle-free router. The CLI host passes `None`.
async fn serve_one_request<B>(
    deps: ServerDeps,
    req: Request<B>,
    app: Option<&AppHandle>,
    exposure: ListenerExposure,
    primary_routes: bool,
    remote_address: IpAddr,
) -> Result<(Response<ResponseBody>, Option<String>), Infallible>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    let auth_family = if primary_routes {
        classify_bearer_family(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        )
    } else {
        // A policy-only loopback endpoint has the same route surface as a LAN
        // endpoint. This is an ownership selector only; the M3 service still
        // performs real authentication before any model/runtime probe.
        AuthFamily::PairedLanToken
    };
    let decision = classify_request(
        &method,
        &path,
        ClassificationInput::new(exposure, auth_family),
    );
    let mut route = match decision {
        RouteDecision::Allowed(route) => Some(route),
        RouteDecision::MethodNotAllowed {
            route,
            owner: RouteOwner::M3,
            ..
        } => {
            let response =
                dispatch_m3_route(&deps, route, method, headers, Bytes::new(), remote_address)
                    .await;
            return Ok((response, None));
        }
        RouteDecision::MethodNotAllowed { .. }
            if primary_routes && auth_family != AuthFamily::PairedLanToken =>
        {
            // Byte-compatible migration path: legacy authenticated every
            // non-preflight request before falling through to its 404. A typed
            // 405 here would become both a route oracle and a wire regression.
            None
        }
        RouteDecision::MethodNotAllowed { .. } => {
            let response = deps
                .m3_policy
                .as_deref()
                .map(|policy| crate::m3_http_server::route_not_found_response(policy, &headers))
                .unwrap_or_else(|| with_cors(not_found_response()));
            return Ok((response, None));
        }
        RouteDecision::Denied(_) | RouteDecision::NotFound
            if primary_routes && auth_family != AuthFamily::PairedLanToken =>
        {
            // Keep denied capabilities non-dispatchable, but preserve legacy
            // auth/body/preflight ordering and its exact 401/404/204 bytes.
            None
        }
        RouteDecision::Denied(_) | RouteDecision::NotFound => {
            // Explicit capability denials and unknown paths are intentionally
            // indistinguishable, and neither allocates a request body.
            let response = if primary_routes && auth_family != AuthFamily::PairedLanToken {
                with_cors(not_found_response())
            } else {
                deps.m3_policy
                    .as_deref()
                    .map(|policy| crate::m3_http_server::route_not_found_response(policy, &headers))
                    .unwrap_or_else(|| with_cors(not_found_response()))
            };
            return Ok((response, None));
        }
    };

    if route.is_some_and(|route| {
        route.owner == RouteOwner::Legacy
            && (!primary_routes
                || (auth_family == AuthFamily::PairedLanToken
                    && route.route.family == RouteFamily::LegacyHost))
    }) {
        let response = deps
            .m3_policy
            .as_deref()
            .map(|policy| crate::m3_http_server::route_not_found_response(policy, &headers))
            .unwrap_or_else(|| with_cors(not_found_response()));
        return Ok((response, None));
    }
    if route.is_some_and(|route| route.route.family == RouteFamily::LegacyHost) && app.is_none() {
        // Host routes only exist in the desktop process. The historical CLI
        // treated these paths like any other unknown legacy route, including
        // its authentication/body ordering, so fall through to that core
        // instead of exposing a pre-auth typed-router 404.
        route = None;
    }

    if route.is_some_and(|route| route.owner == RouteOwner::M3) {
        let (Some(service), Some(policy)) = (&deps.m3_service, &deps.m3_policy) else {
            return Ok((with_cors(not_found_response()), None));
        };
        let response = crate::m3_http_server::handle_http_request(
            service
                .clone()
                .with_model_extensions(deps.model_extensions.clone()),
            route.expect("M3 route match").route.id,
            policy.clone(),
            remote_address,
            req,
            M3OperationContext {
                cancellation: deps.cancel.clone(),
                timeout_ms: 30 * 60 * 1_000,
            },
        )
        .await;
        // The M3 routes return here rather than falling through to
        // `handle_request`, so they need their own call — same rule, same
        // filter, one funnel each.
        record_http_request(&deps, &method, &path, response.status());
        return Ok((response, None));
    }

    let public_legacy_request = (method == Method::GET && path == "/health")
        || (method == Method::GET && path == "/v1/contract")
        || (method == Method::OPTIONS && path.starts_with("/v1/"))
        || (app.is_some()
            && matches!(
                extended_route_for(&method, &path),
                Some(ExtendedRoute::LocalAppStatic { .. })
            ));
    if !public_legacy_request {
        let preflight = match extended_route_for(&method, &path) {
            Some(ExtendedRoute::LocalAppRun(app_id)) if app.is_some() => {
                authenticate_local_app_token(&deps, &headers, &app_id).map(Some)
            }
            _ => authenticate_credential(&deps, &headers),
        };
        if let Err(response) = preflight {
            return Ok((with_cors(response), None));
        }
    }

    let body = if public_legacy_request {
        // Health, preflight, and published static pages never consume a
        // request envelope. Dropping the body here prevents an unauthenticated
        // caller from forcing the listener to buffer 32 MiB for a public route.
        Bytes::new()
    } else {
        match read_capped_body(req.into_body(), MAX_REQUEST_BODY_BYTES, &deps.cancel).await {
            Ok(bytes) => bytes,
            Err(response) => return Ok((response, None)),
        }
    };

    if let Some(app) = app {
        if let Some(result) =
            handle_extended_request(app, &deps, &method, &path, &headers, &body).await
        {
            // The desktop-only routes also return early. `LocalAppRun` is the
            // one HTTP route that *is* permission-gated, so leaving it out would
            // mean the stream missed the request most worth having.
            record_http_request(&deps, &method, &path, result.0.status());
            return Ok(result);
        }
    }

    Ok(handle_request(
        &deps,
        ServerRequest {
            method,
            path,
            query,
            headers,
            body,
        },
    )
    .await)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointRequestSurface {
    Legacy,
    M3,
    HiddenLegacy,
    HiddenM3,
}

fn endpoint_request_surface(
    endpoint: &UnifiedEndpoint,
    has_m3_policy: bool,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> EndpointRequestSurface {
    let auth_family = if endpoint.primary {
        classify_bearer_family(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        )
    } else {
        AuthFamily::PairedLanToken
    };
    match classify_request(
        method,
        path,
        ClassificationInput::new(endpoint.exposure, auth_family),
    ) {
        RouteDecision::Allowed(route) if route.owner == RouteOwner::M3 => {
            if has_m3_policy {
                EndpointRequestSurface::M3
            } else {
                EndpointRequestSurface::HiddenLegacy
            }
        }
        RouteDecision::MethodNotAllowed {
            owner: RouteOwner::M3,
            ..
        } => {
            if has_m3_policy {
                EndpointRequestSurface::M3
            } else {
                EndpointRequestSurface::HiddenLegacy
            }
        }
        RouteDecision::Allowed(route)
            if route.owner == RouteOwner::Legacy
                && endpoint.primary
                && !(auth_family == AuthFamily::PairedLanToken
                    && route.route.family == RouteFamily::LegacyHost) =>
        {
            EndpointRequestSurface::Legacy
        }
        RouteDecision::MethodNotAllowed {
            owner: RouteOwner::Legacy,
            ..
        } if endpoint.primary && auth_family != AuthFamily::PairedLanToken => {
            EndpointRequestSurface::Legacy
        }
        RouteDecision::Allowed(_) | RouteDecision::MethodNotAllowed { .. } => {
            EndpointRequestSurface::HiddenM3
        }
        RouteDecision::Denied(_) | RouteDecision::NotFound
            if endpoint.primary && auth_family != AuthFamily::PairedLanToken =>
        {
            // Unknown primary routes still owe the legacy listener's
            // auth/body/preflight ordering and exact response bytes.
            EndpointRequestSurface::Legacy
        }
        RouteDecision::Denied(_) | RouteDecision::NotFound
            if !endpoint.primary || auth_family == AuthFamily::PairedLanToken =>
        {
            EndpointRequestSurface::HiddenM3
        }
        RouteDecision::Denied(_) | RouteDecision::NotFound => EndpointRequestSurface::HiddenLegacy,
    }
}

fn endpoint_refusal_response(
    surface: EndpointRequestSurface,
    policy: Option<&crate::compatibility_hub::LanServerPolicy>,
    headers: &HeaderMap,
) -> Response<ResponseBody> {
    match surface {
        EndpointRequestSurface::Legacy => with_cors(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "The API server active-request quota is exhausted",
            "server_busy",
        )),
        EndpointRequestSurface::M3 => policy
            .map(|policy| crate::m3_http_server::server_busy_response(policy, headers))
            .unwrap_or_else(|| with_cors(not_found_response())),
        EndpointRequestSurface::HiddenM3 => policy
            .map(|policy| crate::m3_http_server::route_not_found_response(policy, headers))
            .unwrap_or_else(|| with_cors(not_found_response())),
        EndpointRequestSurface::HiddenLegacy => with_cors(not_found_response()),
    }
}

async fn serve_endpoint_connection<I>(
    io: I,
    host: EndpointHost,
    endpoint: UnifiedEndpoint,
    remote_address: IpAddr,
    admission: Arc<crate::http_policy::RequestAdmission>,
    shutdown: tokio_util::sync::CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection_shutdown = shutdown.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let host = host.clone();
        let endpoint = endpoint.clone();
        let admission = admission.clone();
        let shutdown = shutdown.clone();
        async move {
            let endpoint_policy = host.m3_policy();
            let surface = endpoint_request_surface(
                &endpoint,
                endpoint_policy.is_some(),
                req.method(),
                req.uri().path(),
                req.headers(),
            );
            if matches!(
                surface,
                EndpointRequestSurface::HiddenLegacy | EndpointRequestSurface::HiddenM3
            ) {
                let response =
                    endpoint_refusal_response(surface, endpoint_policy.as_deref(), req.headers());
                host.record_hidden_response(response.status());
                return Ok::<_, Infallible>(response);
            }
            let refused =
                endpoint_refusal_response(surface, endpoint_policy.as_deref(), req.headers());
            let request_host = host.clone();
            let response = serve_with_admission_response(
                &admission,
                &shutdown,
                refused,
                |cancel| async move {
                    request_host
                        .serve_request(req, &endpoint, remote_address, cancel)
                        .await
                },
            )
            .await;
            Ok::<_, Infallible>(host.after_response_ends(response))
        }
    });
    let connection = http1::Builder::new().serve_connection(TokioIo::new(io), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection_shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
        _ = &mut connection => {}
    }
}

/// One endpoint accept task. It owns and joins every connection task it
/// creates, so awaiting this handle means both the listening socket and all
/// already-admitted HTTP work are gone.
async fn run_unified_endpoint(
    host: EndpointHost,
    listener: TcpListener,
    endpoint: UnifiedEndpoint,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    admission: Arc<crate::http_policy::RequestAdmission>,
    connection_limit: Arc<Semaphore>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
            accepted = listener.accept() => {
                let (stream, remote) = match accepted {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let connection_permit = match connection_limit.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };
                let host = host.clone();
                let endpoint = endpoint.clone();
                let admission = admission.clone();
                let shutdown = shutdown.clone();
                if let Some(acceptor) = tls_acceptor.clone() {
                    connections.spawn(async move {
                        let _connection_permit = connection_permit;
                        let accepted = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            acceptor.accept(stream),
                        ).await;
                        if let Ok(Ok(stream)) = accepted {
                            serve_endpoint_connection(
                                stream,
                                host,
                                endpoint,
                                remote.ip(),
                                admission,
                                shutdown,
                            ).await;
                        }
                    });
                } else {
                    connections.spawn(async move {
                        let _connection_permit = connection_permit;
                        serve_endpoint_connection(
                            stream,
                            host,
                            endpoint,
                            remote.ip(),
                            admission,
                            shutdown,
                        ).await;
                    });
                }
            }
        }
    }

    // Connection futures observe the same cancellation token and ask Hyper
    // for graceful shutdown. Bound the drain so a malicious peer that never
    // finishes TLS/HTTP teardown cannot wedge application exit forever.
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

/// Binds the headless CLI's loopback endpoint. Desktop generations bind their
/// reconciled endpoint candidates through [`bind_candidate`]. Keeping this
/// helper separate also makes the legacy "port already in use" failure path
/// directly testable without an `AppHandle` — see
/// `tests::bind_conflict_surfaces_as_status_error`.
async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| {
            crate::http_policy::describe_bind_error(
                crate::http_policy::ListenerRole::LegacyProxy,
                "127.0.0.1",
                port,
                &error,
            )
        })
}

// ---------------------------------------------------------------------
// monkey-cli `api-serve` (design doc phase 4): the SAME routing/proxy core
// (`ServerDeps`/`serve_one_request`/`handle_request`/`bind_listener`/
// `load_config_impl`) the GUI uses, with no `AppHandle` or GUI state. Its
// CLI-local `AppState` only serializes token-use config writes; the surrounding
// bookkeeping differs (stdout/stderr logging instead of `apiserver://status`
// events, an HTTP probe instead of reading `AppState::llama` in-process).
// `monkey-cli`'s `main.rs` resolves the
// `api_server.json`/`providers.json` paths itself (the same
// `APP_IDENTIFIER`-hardcoding technique `providers_cli.rs`/
// `checkpoints_cli.rs` already use) and hands them in here — see the design
// doc's "config drift" risk note: this deliberately reads the SAME
// `api_server.json` the GUI writes, so tokens and toggles set in Settings
// carry over to the CLI and vice versa.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliRuntimeEndpoints {
    pub llama_port: u16,
    pub ollama_base_url: String,
}

impl Default for CliRuntimeEndpoints {
    fn default() -> Self {
        Self {
            llama_port: crate::llama::LlamaState::default().port,
            ollama_base_url: ollama::OLLAMA_BASE_URL.to_string(),
        }
    }
}

/// Runs the local API server as a blocking, headless accept loop — never
/// returns on success (Ctrl+C/SIGINT ends the process the same way `ollama
/// serve`'s passthrough does); returns `Err` only for a bind failure, so
/// `monkey-cli`'s `main` can print it and exit non-zero exactly like every other
/// subcommand's error path (`fail()`).
///
/// `load_custom_providers` is re-invoked on every request (not
/// cached once at startup) for the same "never stale" reasoning
/// [`build_deps`] applies to tokens — a provider added via the GUI's
/// Settings while `api-serve` is already running becomes routable
/// immediately, no CLI restart needed.
pub async fn run_cli_server(
    port: u16,
    config_path: PathBuf,
    load_custom_providers: impl Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static,
) -> Result<(), String> {
    let app_data_dir = config_path
        .parent()
        .ok_or_else(|| "API server config path has no app-data parent".to_string())?
        .to_path_buf();
    let m3_hub = bounded_thread_initialization(
        "unified M3 HTTP service",
        std::time::Duration::from_secs(15),
        move || {
            crate::m3_production::build_m3_command_state(&app_data_dir)
                .map(|state| state.hub.clone())
                .map_err(|error| error.to_string())
        },
    )
    .await?;

    run_cli_server_with_m3_hub(port, config_path, m3_hub, load_custom_providers).await
}

/// Runs one synchronous initializer on its own OS thread, but never leaves
/// async startup waiting on a platform service indefinitely. In particular,
/// macOS Security.framework can block inside a keychain call before yielding
/// control back to Rust; `spawn_blocking` plus an async timeout would still
/// leave a Tokio runtime waiting for that worker during shutdown. A dedicated
/// thread can be detached safely after this bounded channel wait.
async fn bounded_thread_initialization<T, F>(
    label: &'static str,
    budget: std::time::Duration,
    initialize: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("little-monkey-{label}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(initialize))
                .map_err(|_| format!("Could not initialize the {label}: initializer panicked"))
                .and_then(|result| {
                    result.map_err(|error| format!("Could not initialize the {label}: {error}"))
                });
            let _ = sender.send(result);
        })
        .map_err(|error| format!("Could not start the {label} initializer: {error}"))?;

    match tokio::time::timeout(budget, receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(format!(
            "Could not initialize the {label}: initializer thread exited without a result"
        )),
        Err(_) => Err(format!(
            "Could not initialize the {label} within {} seconds; no HTTP listener was opened",
            budget.as_secs()
        )),
    }
}

/// AppHandle-free common CLI server used by production after its bounded M3
/// initialization and by compatibility tests with an explicitly constructed
/// test hub. Injection stops tests from touching the developer's OS keychain;
/// production still fails closed if its real hub cannot initialize, so pairing
/// support is never silently omitted.
pub async fn run_cli_server_with_m3_hub(
    port: u16,
    config_path: PathBuf,
    m3_hub: Arc<M3RuntimeHub>,
    load_custom_providers: impl Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static,
) -> Result<(), String> {
    run_cli_server_with_m3_hub_and_endpoints(
        port,
        config_path,
        m3_hub,
        CliRuntimeEndpoints::default(),
        load_custom_providers,
    )
    .await
}

pub async fn run_cli_server_with_m3_hub_and_endpoints(
    port: u16,
    config_path: PathBuf,
    m3_hub: Arc<M3RuntimeHub>,
    endpoints: CliRuntimeEndpoints,
    load_custom_providers: impl Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static,
) -> Result<(), String> {
    run_cli_server_with_m3_hub_and_connection_limit(
        port,
        config_path,
        m3_hub,
        endpoints,
        load_custom_providers,
        MAX_HTTP_CONNECTIONS,
    )
    .await
}

async fn run_cli_server_with_m3_hub_and_connection_limit(
    port: u16,
    config_path: PathBuf,
    m3_hub: Arc<M3RuntimeHub>,
    endpoints: CliRuntimeEndpoints,
    load_custom_providers: impl Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static,
    max_connections: usize,
) -> Result<(), String> {
    // Two clients, one per policy — see `ServerDeps::local_client` for why one
    // client cannot serve both loopback inference and a credentialed cloud
    // provider. A failure to build the hardened one is fatal rather than a silent
    // fallback to the bare client: falling back would mean serving cloud requests
    // with no redirect policy, which is the hole this split exists to close.
    let local_client = bounded_loopback_client()?;
    let cloud_client = crate::egress::hardened()
        .build()
        .map_err(|error| format!("Failed to build the cloud-provider HTTP client: {error}"))?;
    let llama_port = endpoints.llama_port;
    let ollama_base_url = endpoints.ollama_base_url;
    // Built once for the whole listener. The CLI host does not have the GUI's
    // audit service; it only has the data directory `config_path` sits in, so it
    // opens the ledger itself — see `subsystem_audit`'s module docs for why the
    // contexts differ by what they can reach rather than by taste.
    let audit = match config_path.parent() {
        Some(app_data_dir) => crate::subsystem_audit::SubsystemAudit::in_data_dir(app_data_dir),
        None => crate::subsystem_audit::SubsystemAudit::disabled(
            "the API server config path has no app-data parent to find a ledger in",
        ),
    };
    // Said once at startup, so an operator learns a listener is not auditing now
    // rather than inferring it later from an empty stream.
    eprintln!("API server subsystem audit: {}", audit.describe());
    let configured_m3_policy = m3_hub.lan_policy().map_err(|error| error.to_string())?;
    let model_service = HttpModelService::for_m3_hub(m3_hub.clone());
    let m3_service = M3HttpRequestService::with_model_service(m3_hub, model_service.clone());
    let m3_policy = primary_m3_policy(configured_m3_policy.as_ref(), port);
    // Bind and announce readiness only after every fallible dependency is
    // initialized. A rejected app-data directory or client build must never
    // leave a caller waiting on a listener that can no longer reach its loop.
    let listener = bind_listener(port).await?;
    println!("Little Monkey API server listening on http://127.0.0.1:{port}/v1 (Ctrl+C to stop)");
    // Guards this process's own `api_server.json` read-modify-write cycles
    // for the `last_used_at` bump — a fresh, CLI-local `AppState` is
    // enough to serialize concurrent requests *within this process*; it
    // does not (and can't, being an in-memory lock) protect against a
    // simultaneously-running GUI process also writing the same file. That
    // cross-process race is the same pre-existing "shared JSON file" risk
    // the design doc's "config drift" note already flags — the atomic
    // temp+rename write in `save_config_impl` bounds it to "last writer
    // wins", never a torn file.
    let host = EndpointHost::Cli(Arc::new(CliServerRuntime {
        config_path,
        load_custom_providers: Arc::new(load_custom_providers),
        state: Arc::new(AppState::default()),
        local_client,
        cloud_client,
        llama_port,
        ollama_base_url,
        legacy_rate_limiter: Arc::new(crate::http_policy::LegacyTokenRateLimiter::default()),
        model_service,
        m3_service,
        m3_policy: m3_policy.clone(),
        audit,
    }));
    let admission = Arc::new(crate::http_policy::RequestAdmission::new(
        crate::http_policy::MAX_ACTIVE_REQUESTS,
    ));
    let connection_limit = Arc::new(Semaphore::new(max_connections.max(1)));
    // The CLI has no graceful-stop API, so this token remains live until the
    // command future (and its owned connection tasks) is dropped.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let endpoint = UnifiedEndpoint {
        key: format!("http://127.0.0.1:{port}"),
        bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        port,
        exposure: ListenerExposure::Loopback,
        transport: EndpointTransport::Plaintext,
        primary: true,
        policy: m3_policy.as_deref().cloned(),
    };
    run_unified_endpoint(
        host,
        listener,
        endpoint,
        None,
        admission,
        connection_limit,
        shutdown,
    )
    .await;
    Ok(())
}

/// Tiny formatting helper for [`EndpointHost`]'s CLI request log line.
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

async fn stop_running_unified(state: &UnifiedHttpServerState) -> Result<(), String> {
    let (shutdown, endpoints) = {
        let mut inner = state.lock()?;
        inner.status = "stopping".to_string();
        (inner.shutdown.take(), std::mem::take(&mut inner.endpoints))
    };
    if let Some(shutdown) = shutdown {
        shutdown.cancel();
    }
    for endpoint in endpoints {
        let _ = endpoint.task.await;
    }
    let mut inner = state.lock()?;
    inner.status = "stopped".to_string();
    inner.started_at_ms = None;
    Ok(())
}

fn sync_api_server_projection(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    let unified = app.state::<UnifiedHttpServerState>().snapshot()?;
    let state = app.state::<AppState>();
    let payload = {
        let mut api = state.api_server.lock().map_err(|error| error.to_string())?;
        if let Some(primary) = unified.endpoints.iter().find(|endpoint| endpoint.primary) {
            api.port = primary.port;
        }
        api.status = if unified.primary_enabled {
            unified.status.clone()
        } else {
            "stopped".to_string()
        };
        api.request_count = unified.request_count;
        api.last_request_at = unified.last_request_at_ms;
        api.last_error = unified
            .primary_enabled
            .then(|| unified.last_error.clone())
            .flatten();
        status_payload(&api)
    };
    emit_status(app, &payload);
    Ok(payload)
}

fn record_unified_error(state: &UnifiedHttpServerState, message: &str) -> Result<(), String> {
    let mut inner = state.lock()?;
    inner.last_error = Some(message.to_string());
    if inner.endpoints.is_empty() {
        inner.status = "error".to_string();
        inner.started_at_ms = None;
    }
    Ok(())
}

struct PreparedEndpoint {
    endpoint: UnifiedEndpoint,
    listener: TcpListener,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
}

#[derive(Clone)]
struct EndpointCandidate {
    endpoint: UnifiedEndpoint,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
}

struct PreviousGeneration {
    spec: Option<UnifiedGenerationSpec>,
    status: String,
    generation: u64,
    started_at_ms: Option<u64>,
    last_error: Option<String>,
    primary_enabled: bool,
    policy_enabled: bool,
    admission: Arc<crate::http_policy::RequestAdmission>,
    had_endpoints: bool,
}

fn applied_generation_is_healthy(
    inner: &crate::unified_http_server::UnifiedServerInner,
    desired: &[EndpointCandidate],
) -> bool {
    if desired.is_empty() {
        return inner.endpoints.is_empty() && inner.shutdown.is_none() && inner.status == "stopped";
    }
    if inner.status != "running"
        || inner.endpoints.len() != desired.len()
        || inner
            .shutdown
            .as_ref()
            .is_none_or(tokio_util::sync::CancellationToken::is_cancelled)
        || inner
            .endpoints
            .iter()
            .any(|endpoint| endpoint.task.is_finished())
    {
        return false;
    }
    desired.iter().all(|candidate| {
        inner
            .endpoints
            .iter()
            .any(|running| running.endpoint == candidate.endpoint)
    })
}

fn generation_spec(
    config: &ApiServerConfig,
    configured_policy: Option<crate::compatibility_hub::LanServerPolicy>,
    primary_enabled: bool,
    policy_enabled: bool,
) -> Result<UnifiedGenerationSpec, String> {
    if policy_enabled && configured_policy.is_none() {
        return Err("Configure an M3 LAN policy before starting its HTTP endpoint".to_string());
    }
    Ok(UnifiedGenerationSpec {
        primary: primary_enabled.then(|| PrimaryServiceConfig {
            port: config.port,
            require_token: config.require_token,
            expose_ollama: config.expose_ollama,
            expose_providers: config.expose_providers,
        }),
        policy_endpoint: policy_enabled.then(|| configured_policy.clone()).flatten(),
        pairing_policy: configured_policy,
    })
}

async fn endpoint_candidates(
    spec: &UnifiedGenerationSpec,
) -> Result<Vec<EndpointCandidate>, String> {
    let plan = spec.endpoint_plan()?;
    let mut candidates = Vec::with_capacity(plan.endpoints.len());
    for endpoint in plan.endpoints {
        let tls_acceptor = match endpoint.transport {
            EndpointTransport::Plaintext => None,
            EndpointTransport::Tls => {
                let policy = endpoint
                    .policy
                    .as_ref()
                    .ok_or_else(|| "A TLS endpoint requires an M3 LAN policy".to_string())?;
                crate::m3_http_server::tls_acceptor(policy).await?
            }
        };
        candidates.push(EndpointCandidate {
            endpoint,
            tls_acceptor,
        });
    }
    Ok(candidates)
}

fn runtime_for_spec(
    hub: Arc<crate::m3_runtime_hub::M3RuntimeHub>,
    spec: &UnifiedGenerationSpec,
) -> Result<ServerRuntime, String> {
    let cloud_client = crate::egress::hardened()
        .build()
        .map_err(|error| format!("Failed to build the cloud-provider HTTP client: {error}"))?;
    let primary = spec.primary.as_ref();
    let model_service = HttpModelService::for_m3_hub(hub.clone());
    Ok(ServerRuntime {
        local_client: bounded_loopback_client()?,
        cloud_client,
        ollama_base_url: ollama::OLLAMA_BASE_URL.to_string(),
        require_token: primary.is_none_or(|config| config.require_token),
        expose_ollama: primary.is_some_and(|config| config.expose_ollama),
        expose_providers: primary.is_some_and(|config| config.expose_providers),
        legacy_rate_limiter: Arc::new(crate::http_policy::LegacyTokenRateLimiter::default()),
        model_service: model_service.clone(),
        m3_service: Some(M3HttpRequestService::with_model_service(hub, model_service)),
        m3_policy: None,
    })
}

async fn bind_candidate(candidate: EndpointCandidate) -> Result<PreparedEndpoint, String> {
    let endpoint = candidate.endpoint;
    let listener = TcpListener::bind((endpoint.bind_address, endpoint.port))
        .await
        .map_err(|error| {
            let role = if endpoint.primary {
                crate::http_policy::ListenerRole::LegacyProxy
            } else {
                crate::http_policy::ListenerRole::CompatibilityListener
            };
            crate::http_policy::describe_bind_error(
                role,
                &endpoint.bind_address.to_string(),
                endpoint.port,
                &error,
            )
        })?;
    Ok(PreparedEndpoint {
        endpoint,
        listener,
        tls_acceptor: candidate.tls_acceptor,
    })
}

fn spawn_generation(
    app: &AppHandle,
    prepared: Vec<PreparedEndpoint>,
    base_runtime: ServerRuntime,
    pairing_policy: Option<&crate::compatibility_hub::LanServerPolicy>,
    admission: Arc<crate::http_policy::RequestAdmission>,
) -> (tokio_util::sync::CancellationToken, Vec<RunningEndpoint>) {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let connection_limit = Arc::new(Semaphore::new(MAX_HTTP_CONNECTIONS));
    let mut running = Vec::with_capacity(prepared.len());
    for prepared in prepared {
        let mut runtime = base_runtime.clone();
        runtime.m3_policy = match prepared.endpoint.policy.as_ref() {
            Some(policy) => Some(Arc::new(policy.clone())),
            None => primary_m3_policy(pairing_policy, prepared.endpoint.port),
        };
        let endpoint = prepared.endpoint.clone();
        let task = tokio::spawn(run_unified_endpoint(
            EndpointHost::Desktop {
                app: app.clone(),
                runtime: Arc::new(runtime),
            },
            prepared.listener,
            prepared.endpoint,
            prepared.tls_acceptor,
            admission.clone(),
            connection_limit.clone(),
            shutdown.clone(),
        ));
        running.push(RunningEndpoint { endpoint, task });
    }
    (shutdown, running)
}

async fn restore_previous_generation(
    app: &AppHandle,
    state: &UnifiedHttpServerState,
    previous: PreviousGeneration,
    candidates: Vec<EndpointCandidate>,
    runtime: Option<ServerRuntime>,
    desired_error: &str,
) -> Result<(), String> {
    if !previous.had_endpoints {
        let mut inner = state.lock()?;
        inner.status = previous.status;
        inner.generation = previous.generation;
        inner.started_at_ms = previous.started_at_ms;
        inner.last_error = previous.last_error;
        inner.primary_enabled = previous.primary_enabled;
        inner.policy_enabled = previous.policy_enabled;
        inner.applied_spec = previous.spec;
        inner.admission = previous.admission;
        return Ok(());
    }
    let spec = previous
        .spec
        .clone()
        .ok_or_else(|| "running HTTP generation has no rollback specification".to_string())?;
    let runtime =
        runtime.ok_or_else(|| "running HTTP generation has no rollback runtime".to_string())?;
    let mut prepared = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match bind_candidate(candidate).await {
            Ok(endpoint) => prepared.push(endpoint),
            Err(rollback_error) => {
                drop(prepared);
                let combined = format!(
                    "HTTP reconciliation failed ({desired_error}); restoring the previous generation also failed: {rollback_error}"
                );
                let mut inner = state.lock()?;
                inner.status = "error".to_string();
                inner.last_error = Some(combined.clone());
                inner.started_at_ms = None;
                inner.primary_enabled = previous.primary_enabled;
                inner.policy_enabled = previous.policy_enabled;
                inner.applied_spec = previous.spec;
                inner.admission = previous.admission;
                return Err(combined);
            }
        }
    }
    let (shutdown, endpoints) = spawn_generation(
        app,
        prepared,
        runtime,
        spec.pairing_policy.as_ref(),
        previous.admission.clone(),
    );
    let mut inner = state.lock()?;
    inner.status = previous.status;
    inner.generation = previous.generation;
    inner.started_at_ms = previous.started_at_ms;
    inner.last_error = previous.last_error;
    inner.primary_enabled = previous.primary_enabled;
    inner.policy_enabled = previous.policy_enabled;
    inner.applied_spec = previous.spec;
    inner.admission = previous.admission;
    inner.shutdown = Some(shutdown);
    inner.endpoints = endpoints;
    Ok(())
}

async fn reconcile_unified_server_locked(
    app: &AppHandle,
    primary_enabled: Option<bool>,
    policy_enabled: Option<bool>,
) -> Result<(), String> {
    let state = app.state::<UnifiedHttpServerState>();
    let (primary_enabled, policy_enabled) = {
        let inner = state.lock()?;
        (
            primary_enabled.unwrap_or(inner.primary_enabled),
            policy_enabled.unwrap_or(inner.policy_enabled),
        )
    };

    let config = load_config_impl(&config_file_path(app)?)?;
    let hub = app
        .state::<crate::m3_commands::M3CommandState>()
        .hub
        .clone();
    let configured_policy = hub.lan_policy().map_err(|error| error.to_string())?;
    let desired_spec = match generation_spec(
        &config,
        configured_policy.clone(),
        primary_enabled,
        policy_enabled,
    ) {
        Ok(spec) => spec,
        Err(error) => {
            record_unified_error(&state, &error)?;
            let _ = sync_api_server_projection(app);
            return Err(error);
        }
    };
    let desired_candidates = match endpoint_candidates(&desired_spec).await {
        Ok(candidates) => candidates,
        Err(error) => {
            record_unified_error(&state, &error)?;
            let _ = sync_api_server_projection(app);
            return Err(error);
        }
    };
    let desired_runtime = match runtime_for_spec(hub.clone(), &desired_spec) {
        Ok(runtime) => runtime,
        Err(error) => {
            record_unified_error(&state, &error)?;
            let _ = sync_api_server_projection(app);
            return Err(error);
        }
    };
    let previous = {
        let inner = state.lock()?;
        if inner.applied_spec.as_ref() == Some(&desired_spec)
            && applied_generation_is_healthy(&inner, &desired_candidates)
        {
            return Ok(());
        }
        PreviousGeneration {
            spec: inner.applied_spec.clone(),
            status: inner.status.clone(),
            generation: inner.generation,
            started_at_ms: inner.started_at_ms,
            last_error: inner.last_error.clone(),
            primary_enabled: inner.primary_enabled,
            policy_enabled: inner.policy_enabled,
            admission: inner.admission.clone(),
            had_endpoints: !inner.endpoints.is_empty(),
        }
    };
    let old_socket_set = {
        let inner = state.lock()?;
        inner
            .endpoints
            .iter()
            .map(|running| (running.endpoint.bind_address, running.endpoint.port))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let (rollback_candidates, rollback_runtime) = if let Some(spec) = previous.spec.as_ref() {
        let candidates = match endpoint_candidates(spec).await {
            Ok(candidates) => candidates,
            Err(error) => {
                record_unified_error(&state, &error)?;
                let _ = sync_api_server_projection(app);
                return Err(format!("Could not prepare a safe HTTP rollback: {error}"));
            }
        };
        let runtime = match runtime_for_spec(hub, spec) {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                record_unified_error(&state, &error)?;
                let _ = sync_api_server_projection(app);
                return Err(format!("Could not prepare a safe HTTP rollback: {error}"));
            }
        };
        (candidates, runtime)
    } else {
        (Vec::new(), None)
    };

    // Bind every non-conflicting desired socket while the healthy generation
    // is still live. A normal port change therefore fails without downtime.
    let mut prepared = Vec::new();
    let mut deferred = Vec::new();
    for candidate in desired_candidates {
        let socket = (candidate.endpoint.bind_address, candidate.endpoint.port);
        if old_socket_set.contains(&socket) {
            deferred.push(candidate);
        } else {
            match bind_candidate(candidate).await {
                Ok(endpoint) => prepared.push(endpoint),
                Err(error) => {
                    drop(prepared);
                    record_unified_error(&state, &error)?;
                    let _ = sync_api_server_projection(app);
                    return Err(error);
                }
            }
        }
    }

    stop_running_unified(&state).await?;
    for candidate in deferred {
        match bind_candidate(candidate).await {
            Ok(endpoint) => prepared.push(endpoint),
            Err(error) => {
                drop(prepared);
                let rollback = restore_previous_generation(
                    app,
                    &state,
                    previous,
                    rollback_candidates,
                    rollback_runtime,
                    &error,
                )
                .await;
                let _ = sync_api_server_projection(app);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(rollback_error),
                };
            }
        }
    }

    if prepared.is_empty() {
        let mut inner = state.lock()?;
        inner.generation = inner.generation.saturating_add(1);
        inner.primary_enabled = desired_spec.primary_enabled();
        inner.policy_enabled = desired_spec.policy_enabled();
        inner.applied_spec = Some(desired_spec);
        inner.last_error = None;
        inner.admission = Arc::new(crate::http_policy::RequestAdmission::new(
            crate::http_policy::MAX_ACTIVE_REQUESTS,
        ));
        drop(inner);
        let _ = sync_api_server_projection(app)?;
        return Ok(());
    }

    let admission = Arc::new(crate::http_policy::RequestAdmission::new(
        crate::http_policy::MAX_ACTIVE_REQUESTS,
    ));
    let (shutdown, running) = spawn_generation(
        app,
        prepared,
        desired_runtime,
        desired_spec.pairing_policy.as_ref(),
        admission.clone(),
    );
    {
        let mut inner = state.lock()?;
        inner.generation = inner.generation.saturating_add(1);
        inner.primary_enabled = desired_spec.primary_enabled();
        inner.policy_enabled = desired_spec.policy_enabled();
        inner.applied_spec = Some(desired_spec);
        inner.status = "running".to_string();
        inner.started_at_ms = Some(now_ms());
        inner.last_error = None;
        inner.shutdown = Some(shutdown);
        inner.endpoints = running;
        inner.admission = admission;
    }
    let _ = sync_api_server_projection(app)?;
    Ok(())
}

async fn reconcile_unified_server(
    app: &AppHandle,
    primary_enabled: Option<bool>,
    policy_enabled: Option<bool>,
) -> Result<(), String> {
    let state = app.state::<UnifiedHttpServerState>();
    let _lifecycle = state.lifecycle.lock().await;
    reconcile_unified_server_locked(app, primary_enabled, policy_enabled).await
}

pub async fn autostart_unified_server(app: AppHandle) -> Result<(), String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    let has_policy = app
        .state::<crate::m3_commands::M3CommandState>()
        .hub
        .lan_policy()
        .map_err(|error| error.to_string())?
        .is_some();
    reconcile_unified_server(&app, Some(config.autostart), Some(has_policy)).await
}

pub async fn shutdown_unified_server(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<UnifiedHttpServerState>();
    let _lifecycle = state.lifecycle.lock().await;
    {
        let mut inner = state.lock()?;
        inner.primary_enabled = false;
        inner.policy_enabled = false;
    }
    stop_running_unified(&state).await?;
    let _ = sync_api_server_projection(app)?;
    Ok(())
}

async fn start_server_core(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    reconcile_unified_server(app, Some(true), None).await?;
    sync_api_server_projection(app)
}

async fn stop_server_core(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    reconcile_unified_server(app, Some(false), None).await?;
    sync_api_server_projection(app)
}

pub(crate) async fn m3_http_server_start_core(
    app: &AppHandle,
) -> Result<crate::m3_http_server::M3HttpServerStatus, String> {
    reconcile_unified_server(app, None, Some(true)).await?;
    m3_http_server_status_core(app)
}

pub(crate) async fn m3_http_server_stop_core(
    app: &AppHandle,
) -> Result<crate::m3_http_server::M3HttpServerStatus, String> {
    reconcile_unified_server(app, None, Some(false)).await?;
    m3_http_server_status_core(app)
}

fn restore_m3_policy(
    hub: &M3RuntimeHub,
    previous: Option<crate::compatibility_hub::LanServerPolicy>,
) -> Result<(), String> {
    match previous {
        Some(policy) => hub
            .configure_lan(policy)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        None => hub
            .disable_lan("DISABLE LAN API")
            .map(|_| ())
            .map_err(|error| error.to_string()),
    }
}

async fn rollback_m3_policy_transaction(
    app: &AppHandle,
    hub: &M3RuntimeHub,
    previous_policy: Option<crate::compatibility_hub::LanServerPolicy>,
    previous_policy_enabled: bool,
    original_error: String,
) -> String {
    if let Err(error) = restore_m3_policy(hub, previous_policy) {
        return format!(
            "{original_error}; restoring the previous M3 LAN policy also failed: {error}"
        );
    }
    match reconcile_unified_server_locked(app, None, Some(previous_policy_enabled)).await {
        Ok(()) => original_error,
        Err(error) => format!(
            "{original_error}; restoring the previous unified HTTP generation also failed: {error}"
        ),
    }
}

/// Persists a LAN policy and reconciles every affected HTTP endpoint while
/// holding the one lifecycle lock. A concurrent start/stop command therefore
/// cannot observe the new hub policy with the old listener generation. On any
/// listener failure, both the persisted policy and generation are restored.
pub async fn configure_m3_policy_and_reconcile(
    app: &AppHandle,
    policy: crate::compatibility_hub::LanServerPolicy,
) -> Result<crate::compatibility_hub::LanServerPolicy, String> {
    let state = app.state::<UnifiedHttpServerState>();
    let _lifecycle = state.lifecycle.lock().await;
    let command_state = app.state::<crate::m3_commands::M3CommandState>();
    let hub = command_state.hub.clone();
    let previous_policy = hub.lan_policy().map_err(|error| error.to_string())?;
    let previous_policy_enabled = state.lock()?.policy_enabled;
    let configured = hub
        .configure_lan(policy)
        .map_err(|error| error.to_string())?;
    if let Err(error) = reconcile_unified_server_locked(app, None, Some(true)).await {
        return Err(rollback_m3_policy_transaction(
            app,
            &hub,
            previous_policy,
            previous_policy_enabled,
            error,
        )
        .await);
    }
    Ok(configured)
}

/// Disables LAN policy and its endpoint as one serialized transaction. The
/// exact-confirmation check remains owned by `M3RuntimeHub`; no listener is
/// changed unless that mutation succeeds.
pub async fn disable_m3_policy_and_reconcile(
    app: &AppHandle,
    confirmation: &str,
) -> Result<bool, String> {
    let state = app.state::<UnifiedHttpServerState>();
    let _lifecycle = state.lifecycle.lock().await;
    let command_state = app.state::<crate::m3_commands::M3CommandState>();
    let hub = command_state.hub.clone();
    let previous_policy = hub.lan_policy().map_err(|error| error.to_string())?;
    let previous_policy_enabled = state.lock()?.policy_enabled;
    let existed = hub
        .disable_lan(confirmation)
        .map_err(|error| error.to_string())?;
    if let Err(error) = reconcile_unified_server_locked(app, None, Some(false)).await {
        return Err(rollback_m3_policy_transaction(
            app,
            &hub,
            previous_policy,
            previous_policy_enabled,
            error,
        )
        .await);
    }
    Ok(existed)
}

pub(crate) fn m3_http_server_status_core(
    app: &AppHandle,
) -> Result<crate::m3_http_server::M3HttpServerStatus, String> {
    let unified = app.state::<UnifiedHttpServerState>().snapshot()?;
    if !unified.policy_enabled {
        return Ok(crate::m3_http_server::M3HttpServerStatus {
            status: "stopped".to_string(),
            bind_address: None,
            port: None,
            tls: false,
            started_at_ms: None,
            request_count: unified.request_count,
            active_requests: unified.active_requests,
            last_request_at_ms: unified.last_request_at_ms,
            last_error: None,
        });
    }
    let policy = app
        .state::<crate::m3_commands::M3CommandState>()
        .hub
        .lan_policy()
        .map_err(|error| error.to_string())?;
    let endpoint = policy.as_ref().and_then(|policy| {
        unified.endpoints.iter().find(|endpoint| {
            endpoint.bind_address == policy.bind_address && endpoint.port == policy.port
        })
    });
    Ok(crate::m3_http_server::M3HttpServerStatus {
        status: unified.status,
        bind_address: endpoint
            .map(|endpoint| endpoint.bind_address.clone())
            .or_else(|| policy.as_ref().map(|policy| policy.bind_address.clone())),
        port: endpoint
            .map(|endpoint| endpoint.port)
            .or_else(|| policy.as_ref().map(|policy| policy.port)),
        tls: endpoint.map(|endpoint| endpoint.tls).unwrap_or_else(|| {
            policy.as_ref().is_some_and(|policy| {
                matches!(
                    policy.tls,
                    crate::compatibility_hub::TlsPolicy::Certificate { .. }
                )
            })
        }),
        started_at_ms: unified.started_at_ms,
        request_count: unified.request_count,
        active_requests: unified.active_requests,
        last_request_at_ms: unified.last_request_at_ms,
        last_error: unified.last_error,
    })
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
pub fn api_server_status(app: AppHandle) -> Result<ApiServerStatusPayload, String> {
    sync_api_server_projection(&app)
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
) -> Result<(ApiServerConfig, ApiServerConfig, bool), String> {
    if config.port == 0 {
        return Err("Port must be between 1 and 65535".to_string());
    }

    let (previous, updated) = {
        let _guard = state
            .api_server_config_lock
            .lock()
            .map_err(|_| "API server config lock poisoned".to_string())?;
        let mut existing = load_config_impl(path)?;
        let previous = existing.clone();
        existing.port = config.port;
        existing.autostart = config.autostart;
        existing.require_token = config.require_token;
        existing.expose_ollama = config.expose_ollama;
        existing.expose_providers = config.expose_providers;
        save_config_impl(path, &existing)?;
        (previous, existing)
    };

    let needs_restart = {
        let s = state.api_server.lock().map_err(|e| e.to_string())?;
        s.status == "running" || s.status == "starting"
    };

    Ok((previous, updated, needs_restart))
}

fn restore_config_runtime_fields(
    state: &AppState,
    path: &Path,
    previous: &ApiServerConfig,
) -> Result<(), String> {
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut current = load_config_impl(path)?;
    current.port = previous.port;
    current.autostart = previous.autostart;
    current.require_token = previous.require_token;
    current.expose_ollama = previous.expose_ollama;
    current.expose_providers = previous.expose_providers;
    save_config_impl(path, &current)
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
    let path = config_file_path(&app)?;
    let (previous, updated, needs_restart) =
        set_config_with_state_impl(state.inner(), &path, config)?;
    if needs_restart {
        if let Err(error) = reconcile_unified_server(&app, None, None).await {
            return match restore_config_runtime_fields(state.inner(), &path, &previous) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(format!(
                    "{error}; restoring the previous API server config also failed: {rollback_error}"
                )),
            };
        }
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
    use http_body_util::Full;
    use std::io::{Read, Write};

    fn record_bind_error(state: &mut ApiServerState, message: String) {
        state.status = "error".to_string();
        state.last_error = Some(message);
    }

    fn test_provider(id: &str, base_url: &str) -> ProviderSummary {
        ProviderSummary {
            id: id.to_string(),
            base_url: base_url.to_string(),
        }
    }

    /// Which inbound requests reach the subsystem event stream. The rule is a
    /// pure function precisely so it can be pinned here rather than inferred
    /// from whichever handlers somebody remembered to instrument.
    #[test]
    fn only_requests_that_act_are_recorded() {
        // Acts on this machine at someone else's request: recorded.
        for (method, path) in [
            (Method::POST, "/v1/chat/completions"),
            (Method::POST, "/v1/embeddings"),
            (Method::POST, "/v1/knowledge/query"),
        ] {
            assert_eq!(
                http_action_worth_recording(&method, path).as_deref(),
                Some(format!("{method} {path}").as_str()),
                "{method} {path} takes an action and must be recorded"
            );
        }

        // Discovery and liveness: no effect, and every client sends them before
        // the request that does act.
        for (method, path) in [
            (Method::GET, "/health"),
            (Method::OPTIONS, "/v1/chat/completions"),
            (Method::GET, "/v1/models"),
        ] {
            assert!(
                http_action_worth_recording(&method, path).is_none(),
                "{method} {path} carries no effect and must not be recorded"
            );
        }

        // A `POST` to the models route would not be discovery, so the skip is
        // bound to the method too rather than to the path alone.
        assert!(http_action_worth_recording(&Method::POST, "/v1/models").is_some());
    }

    /// An unauthenticated caller is `denied`, not `failed`: a reader counting
    /// failures must not be counting refusals.
    #[test]
    fn refusals_and_errors_are_different_outcomes() {
        use crate::run_ledger::SubsystemOutcome;
        assert_eq!(http_outcome(StatusCode::OK), SubsystemOutcome::Succeeded);
        assert_eq!(
            http_outcome(StatusCode::NO_CONTENT),
            SubsystemOutcome::Succeeded
        );
        assert_eq!(
            http_outcome(StatusCode::UNAUTHORIZED),
            SubsystemOutcome::Denied
        );
        assert_eq!(
            http_outcome(StatusCode::FORBIDDEN),
            SubsystemOutcome::Denied
        );
        assert_eq!(
            http_outcome(StatusCode::TOO_MANY_REQUESTS),
            SubsystemOutcome::Failed,
            "rate limiting is the server failing the caller, not refusing them on policy"
        );
        assert_eq!(
            http_outcome(StatusCode::INTERNAL_SERVER_ERROR),
            SubsystemOutcome::Failed
        );
        assert_eq!(
            http_outcome(StatusCode::NOT_FOUND),
            SubsystemOutcome::Failed
        );
    }

    /// A disabled audit is inert: the recording path must run end to end in a
    /// test without a ledger and without panicking.
    #[test]
    fn recording_through_a_disabled_audit_is_inert() {
        let deps = test_deps("http://127.0.0.1:11434".to_string());
        assert!(!deps.audit.is_recording());
        record_http_request(&deps, &Method::POST, "/v1/embeddings", StatusCode::OK);
        record_http_request(&deps, &Method::GET, "/health", StatusCode::OK);
    }

    fn test_deps(ollama_base_url: String) -> ServerDeps {
        let providers = vec![
            test_provider("openai", "https://api.openai.com/v1"),
            test_provider("anthropic", "https://api.anthropic.com/v1"),
        ];
        let local_client = reqwest::Client::new();
        let cloud_client = crate::egress::hardened()
            .build()
            .expect("the hardened client builds");
        let model_extensions = model_extensions(
            LegacyLlamaInventory::Snapshot(Some("qwen2.5-7b-instruct".to_string())),
            true,
            false,
            &ollama_base_url,
            &providers,
            &local_client,
            &cloud_client,
        );
        ServerDeps {
            // Named rather than silent: a test that records nothing must not
            // look like a route that was never wired up.
            audit: crate::subsystem_audit::SubsystemAudit::disabled(
                "server unit test with no ledger",
            ),
            llama_port: 8090,
            llama_ready: true,
            llama_model_stem: Some("qwen2.5-7b-instruct".to_string()),
            llama_embeddings_enabled: false,
            ollama_base_url,
            require_token: false,
            expose_ollama: true,
            expose_providers: false,
            providers,
            tokens: Vec::new(),
            local_client,
            // The real hardened client, not a stand-in: the tests below assert its
            // actual redirect behaviour, so a bare client here would make them pass
            // for the wrong reason.
            cloud_client,
            legacy_rate_limiter: Arc::new(crate::http_policy::LegacyTokenRateLimiter::default()),
            model_service: HttpModelService::from_sources(Vec::new()),
            model_extensions,
            m3_service: None,
            m3_policy: None,
            // Never cancelled, so every existing test keeps asserting the
            // uncancelled path. The cancellation tests build their own token.
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn refresh_test_model_extensions(deps: &mut ServerDeps) {
        deps.model_extensions = model_extensions(
            LegacyLlamaInventory::Snapshot(
                deps.llama_ready
                    .then(|| deps.llama_model_stem.clone())
                    .flatten(),
            ),
            deps.expose_ollama,
            deps.expose_providers,
            &deps.ollama_base_url,
            &deps.providers,
            &deps.local_client,
            &deps.cloud_client,
        );
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

    struct NoPolicyBody {
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl hyper::body::Body for NoPolicyBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Ready(None)
        }
    }

    struct PendingRequestBody {
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl hyper::body::Body for PendingRequestBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn legacy_invalid_bearer_and_public_routes_never_poll_the_transport_body() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens.push(stored_token(
            "legacy",
            "lmk-valid",
            vec![Scope::Chat],
            vec![Backend::Local],
        ));
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header(header::AUTHORIZATION, "Bearer lmk-invalid")
            .body(NoPolicyBody {
                polls: polls.clone(),
            })
            .expect("invalid legacy request");
        let (response, _) = serve_one_request(
            deps,
            request,
            None,
            ListenerExposure::Loopback,
            true,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )
        .await
        .expect("infallible adapter");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 0);

        for (method, path) in [
            (Method::GET, "/health"),
            (Method::OPTIONS, "/v1/chat/completions"),
        ] {
            let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let request = Request::builder()
                .method(method.clone())
                .uri(path)
                .body(NoPolicyBody {
                    polls: polls.clone(),
                })
                .expect("public legacy request");
            let _ = serve_one_request(
                test_deps("http://127.0.0.1:1".to_string()),
                request,
                None,
                ListenerExposure::Loopback,
                true,
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("infallible adapter");
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{method} {path} must drop its unused body without polling"
            );
        }
    }

    #[tokio::test]
    async fn legacy_stalled_upload_is_cancelled_with_exact_503_and_cors() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let deps = test_deps_cancelled_by("http://127.0.0.1:1".to_string(), cancellation.clone());
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .body(PendingRequestBody {
                polls: polls.clone(),
            })
            .expect("pending legacy request");
        let task = tokio::spawn(serve_one_request(
            deps,
            request,
            None,
            ListenerExposure::Loopback,
            true,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        ));
        while polls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        let (response, _) = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("cancelled upload must not stall")
            .expect("transport task")
            .expect("infallible adapter");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("*"))
        );
        let bytes = body_bytes(response).await;
        assert_eq!(
            bytes,
            Bytes::from_static(br#"{"error":{"code":"server_stopping","message":"The API server stopped before this request completed","type":"invalid_request_error"}}"#)
        );
    }

    #[tokio::test]
    async fn primary_without_a_pairing_policy_conceals_every_m3_only_route_before_body_or_hub() {
        for (method, path) in [
            (Method::POST, "/v1/responses"),
            (Method::POST, "/v1/models/download"),
            (Method::POST, "/v1/models/load"),
            (Method::POST, "/v1/models/delete"),
            (Method::POST, "/v1/requests/cancel"),
        ] {
            let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let request = Request::builder()
                .method(method)
                .uri(path)
                .body(NoPolicyBody {
                    polls: polls.clone(),
                })
                .expect("no-policy request");
            let (response, token) = serve_one_request(
                test_deps("http://127.0.0.1:1".to_string()),
                request,
                None,
                ListenerExposure::Loopback,
                true,
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("infallible transport adapter");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(token.is_none());
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{path} must be rejected before the body is polled"
            );
        }
    }

    #[tokio::test]
    async fn legacy_wrong_methods_unknown_denials_and_preflight_keep_baseline_auth_bytes() {
        for (method, path) in [
            (Method::GET, "/v1/chat/completions"),
            (Method::POST, "/health"),
            (Method::GET, "/v1/tool_run_shell"),
            // This is a real desktop host route, but `api-serve` has no
            // AppHandle. It must retain the CLI's historical unknown-route
            // authentication ordering instead of returning a pre-auth 404.
            (Method::POST, "/v1/knowledge/query"),
        ] {
            let mut missing = test_deps("http://127.0.0.1:1".to_string());
            missing.require_token = true;
            missing.tokens.push(stored_token(
                "legacy",
                "lmk-byte-compatible",
                vec![Scope::Chat, Scope::Models],
                vec![Backend::Local],
            ));
            let request = Request::builder()
                .method(method.clone())
                .uri(path)
                .body(Full::new(Bytes::from_static(b"{}")))
                .expect("legacy request");
            let (response, _) = serve_one_request(
                missing,
                request,
                None,
                ListenerExposure::Loopback,
                true,
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("infallible adapter");
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );

            let mut authorized = test_deps("http://127.0.0.1:1".to_string());
            authorized.require_token = true;
            authorized.tokens.push(stored_token(
                "legacy",
                "lmk-byte-compatible",
                vec![Scope::Chat, Scope::Models],
                vec![Backend::Local],
            ));
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, "Bearer lmk-byte-compatible")
                .body(Full::new(Bytes::from_static(b"{}")))
                .expect("authorized legacy request");
            let (response, _) = serve_one_request(
                authorized,
                request,
                None,
                ListenerExposure::Loopback,
                true,
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("infallible adapter");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let value: serde_json::Value =
                serde_json::from_slice(&body_bytes(response).await).expect("legacy 404 JSON");
            assert_eq!(value["error"]["code"], "not_found");
        }

        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri("/v1/tools")
            .body(Full::new(Bytes::new()))
            .expect("legacy preflight");
        let (preflight, _) = serve_one_request(
            test_deps("http://127.0.0.1:1".to_string()),
            request,
            None,
            ListenerExposure::Loopback,
            true,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )
        .await
        .expect("infallible adapter");
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
    }

    #[test]
    fn primary_m3_policy_never_synthesizes_internal_http_authority() {
        assert!(primary_m3_policy(None, 1234).is_none());
        let mut configured = crate::compatibility_hub::LanServerPolicy::default();
        configured.require_authentication = false;
        configured.pairing_required = false;
        let normalized = primary_m3_policy(Some(&configured), 4321).expect("configured policy");
        assert!(normalized.require_authentication);
        assert!(normalized.pairing_required);
        assert_eq!(normalized.bind_address, "127.0.0.1");
        assert_eq!(normalized.port, 4321);
    }

    #[tokio::test]
    async fn unified_stop_cancels_and_joins_every_endpoint_drain_task() {
        let state = UnifiedHttpServerState::default();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut endpoints = Vec::new();
        for port in [12_341, 12_342] {
            let task_shutdown = shutdown.clone();
            let task_drained = drained.clone();
            let task = tokio::spawn(async move {
                task_shutdown.cancelled().await;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                task_drained.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            endpoints.push(RunningEndpoint {
                endpoint: UnifiedEndpoint {
                    key: format!("http://127.0.0.1:{port}"),
                    bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    port,
                    exposure: ListenerExposure::Loopback,
                    transport: EndpointTransport::Plaintext,
                    primary: port == 12_341,
                    policy: None,
                },
                task,
            });
        }
        {
            let mut inner = state.lock().expect("unified state");
            inner.status = "running".to_string();
            inner.shutdown = Some(shutdown);
            inner.endpoints = endpoints;
        }

        stop_running_unified(&state).await.expect("unified stop");

        assert_eq!(drained.load(std::sync::atomic::Ordering::SeqCst), 2);
        let inner = state.lock().expect("unified state");
        assert_eq!(inner.status, "stopped");
        assert!(inner.shutdown.is_none());
        assert!(inner.endpoints.is_empty());
    }

    #[tokio::test]
    async fn identical_generation_is_a_noop_only_while_every_endpoint_task_is_alive() {
        let state = UnifiedHttpServerState::default();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_shutdown.cancelled().await;
        });
        let endpoint = UnifiedEndpoint {
            key: "http://127.0.0.1:12340".to_string(),
            bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 12_340,
            exposure: ListenerExposure::Loopback,
            transport: EndpointTransport::Plaintext,
            primary: true,
            policy: None,
        };
        let candidates = vec![EndpointCandidate {
            endpoint: endpoint.clone(),
            tls_acceptor: None,
        }];
        {
            let mut inner = state.lock().expect("state");
            inner.status = "running".to_string();
            inner.shutdown = Some(shutdown.clone());
            inner.endpoints = vec![RunningEndpoint { endpoint, task }];
            assert!(applied_generation_is_healthy(&inner, &candidates));
            inner.endpoints[0].task.abort();
        }
        tokio::task::yield_now().await;
        {
            let inner = state.lock().expect("state");
            assert!(inner.endpoints[0].task.is_finished());
            assert!(!applied_generation_is_healthy(&inner, &candidates));
        }
        stop_running_unified(&state).await.expect("cleanup");
    }

    #[tokio::test]
    async fn status_projection_runs_after_the_authoritative_admission_counter_completes() {
        let admission = Arc::new(crate::http_policy::RequestAdmission::new(1));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let response = serve_with_admission(&admission, &shutdown, |_| async {
            with_cors(json_response(StatusCode::OK, json!({"ok": true})))
        })
        .await;
        assert_eq!(admission.request_count(), 0, "body is still in flight");

        let observed = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let observed_for_callback = observed.clone();
        let admission_for_callback = admission.clone();
        let response = after_response_ends(response, move || {
            observed_for_callback.store(
                admission_for_callback.request_count(),
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        let _ = body_bytes(response).await;

        assert_eq!(admission.request_count(), 1);
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cli_m3_initialization_timeout_does_not_wait_for_a_stuck_platform_thread() {
        let started = std::time::Instant::now();
        let error = bounded_thread_initialization(
            "test keychain",
            std::time::Duration::from_millis(20),
            || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                Ok::<_, String>(())
            },
        )
        .await
        .expect_err("initializer must time out");
        assert!(error.contains("no HTTP listener was opened"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(150),
            "the detached platform thread must not hold async startup"
        );
    }

    #[tokio::test]
    async fn cli_connection_cap_rejects_an_extra_slow_header_peer_and_recovers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let root = std::env::temp_dir().join(format!(
            "little-monkey-cli-connection-cap-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("test root");
        let download: Arc<dyn crate::m3_runtime_hub::M3DownloadTransport> = Arc::new(
            crate::m3_runtime_hub::ReqwestM3DownloadTransport::new().expect("download transport"),
        );
        let hub = Arc::new(
            M3RuntimeHub::new(
                root.join("m3"),
                crate::m3_runtime_hub::M3HubConfig {
                    storage_quota_bytes: 8 * 1024 * 1024 * 1024,
                    storage_reserve_bytes: 1024 * 1024 * 1024,
                    ..crate::m3_runtime_hub::M3HubConfig::default()
                },
                crate::m3_runtime_hub::M3RuntimeHubDependencies {
                    clock: Arc::new(crate::m3_runtime_hub::SystemM3Clock),
                    hardware: Arc::new(crate::m3_production::SystemM3HardwareProbe),
                    download,
                    catalogs: Vec::new(),
                    runtimes: Vec::new(),
                    runtime_reconciler: None,
                    lan_factory: None,
                },
            )
            .expect("test M3 hub"),
        );
        let upstream_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("fake llama listener");
        let upstream_port = upstream_listener
            .local_addr()
            .expect("fake llama address")
            .port();
        let upstream = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = upstream_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let body: &[u8] = if request[..read]
                        .windows(14)
                        .any(|part| part == b"GET /v1/models")
                    {
                        br#"{"data":[{"id":"connection-cap-model"}]}"#
                    } else {
                        br#"{"status":"ok"}"#
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        let probe = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("port probe");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);
        let config_path = root.join("api_server.json");
        save_config_impl(&config_path, &ApiServerConfig::default()).expect("test config");
        let endpoints = CliRuntimeEndpoints {
            llama_port: upstream_port,
            ollama_base_url: format!("http://127.0.0.1:{upstream_port}"),
        };
        let server = tokio::spawn(run_cli_server_with_m3_hub_and_connection_limit(
            port,
            config_path,
            hub,
            endpoints,
            Vec::<providers::CustomProviderEntry>::new,
            1,
        ));

        // Wait for the listener with a close-delimited request so readiness
        // itself does not retain the single connection permit.
        let mut ready = false;
        for _ in 0..100 {
            if let Ok(Ok(mut stream)) = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                tokio::net::TcpStream::connect(("127.0.0.1", port)),
            )
            .await
            {
                let _ = stream
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let mut bytes = Vec::new();
                if tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    stream.read_to_end(&mut bytes),
                )
                .await
                .is_ok()
                    && bytes.windows(6).any(|window| window == b"200 OK")
                {
                    ready = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ready, "CLI server did not become ready");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let mut slow = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("slow peer");
        slow.write_all(b"GET /health HTTP/1.1\r\nHost:")
            .await
            .expect("partial header");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut excess = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("excess peer connects to backlog");
        excess
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("excess request");
        let mut excess_bytes = [0u8; 128];
        let excess_read = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            excess.read(&mut excess_bytes),
        )
        .await;
        assert!(
            !matches!(excess_read, Ok(Ok(read)) if read > 0),
            "an excess slow-header connection must not allocate a serving task"
        );
        drop(excess);
        drop(slow);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut recovered = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("recovered peer");
        recovered
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("recovered request");
        let mut recovered_bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recovered.read_to_end(&mut recovered_bytes),
        )
        .await
        .expect("recovered response timeout")
        .expect("recovered response");
        assert!(
            recovered_bytes.windows(6).any(|window| window == b"200 OK"),
            "the permit must return when the slow peer disconnects"
        );

        server.abort();
        let _ = server.await;
        upstream.abort();
        let _ = upstream.await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn occupied_new_port_does_not_quiesce_the_healthy_generation() {
        let healthy_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("healthy listener");
        let healthy_port = healthy_listener
            .local_addr()
            .expect("healthy address")
            .port();
        let blocker = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("occupied desired port");
        let desired_port = blocker.local_addr().expect("blocker address").port();
        let state = UnifiedHttpServerState::default();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_shutdown.cancelled().await;
            drop(healthy_listener);
        });
        {
            let mut inner = state.lock().expect("state");
            inner.status = "running".to_string();
            inner.generation = 9;
            inner.primary_enabled = true;
            inner.policy_enabled = false;
            inner.shutdown = Some(shutdown.clone());
            inner.endpoints = vec![RunningEndpoint {
                endpoint: UnifiedEndpoint {
                    key: format!("http://127.0.0.1:{healthy_port}"),
                    bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    port: healthy_port,
                    exposure: ListenerExposure::Loopback,
                    transport: EndpointTransport::Plaintext,
                    primary: true,
                    policy: None,
                },
                task,
            }];
        }
        let candidate = EndpointCandidate {
            endpoint: UnifiedEndpoint {
                key: format!("http://127.0.0.1:{desired_port}"),
                bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port: desired_port,
                exposure: ListenerExposure::Loopback,
                transport: EndpointTransport::Plaintext,
                primary: true,
                policy: None,
            },
            tls_acceptor: None,
        };

        assert!(bind_candidate(candidate).await.is_err());
        {
            let inner = state.lock().expect("state");
            assert_eq!(inner.status, "running");
            assert_eq!(inner.generation, 9);
            assert!(inner.primary_enabled);
            assert!(!inner.policy_enabled);
            assert_eq!(inner.endpoints.len(), 1);
            assert!(!shutdown.is_cancelled());
        }
        drop(blocker);
        stop_running_unified(&state)
            .await
            .expect("cleanup generation");
    }

    #[test]
    fn missing_policy_validation_leaves_applied_flags_unchanged() {
        let state = UnifiedHttpServerState::default();
        {
            let mut inner = state.lock().expect("state");
            inner.status = "running".to_string();
            inner.generation = 4;
            inner.primary_enabled = true;
            inner.policy_enabled = false;
        }
        let result = generation_spec(&ApiServerConfig::default(), None, true, true);
        assert!(result.is_err());
        let inner = state.lock().expect("state");
        assert_eq!(inner.status, "running");
        assert_eq!(inner.generation, 4);
        assert!(inner.primary_enabled);
        assert!(!inner.policy_enabled);
    }

    #[tokio::test]
    async fn saturated_dedup_socket_uses_the_selected_routes_own_busy_envelope() {
        let origin = "https://paired.example";
        let mut policy = crate::compatibility_hub::LanServerPolicy::default();
        policy.port = 12_343;
        policy.cors_allowlist = vec![origin.to_string()];
        let endpoint = UnifiedEndpoint {
            key: "http://127.0.0.1:12343".to_string(),
            bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 12_343,
            exposure: ListenerExposure::Loopback,
            transport: EndpointTransport::Plaintext,
            primary: true,
            policy: Some(policy.clone()),
        };
        let admission = crate::http_policy::RequestAdmission::new(1);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let _held = admission.try_admit(&shutdown).expect("fill admission pool");
        assert!(admission.try_admit(&shutdown).is_none());

        let legacy_headers = HeaderMap::new();
        let legacy_surface =
            endpoint_request_surface(&endpoint, true, &Method::GET, "/v1/models", &legacy_headers);
        assert_eq!(legacy_surface, EndpointRequestSurface::Legacy);
        let legacy = endpoint_refusal_response(legacy_surface, Some(&policy), &legacy_headers);
        assert_eq!(legacy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            legacy
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        let legacy_body: serde_json::Value =
            serde_json::from_slice(&body_bytes(legacy).await).expect("legacy busy JSON");
        assert_eq!(legacy_body["error"]["type"], "invalid_request_error");

        let mut paired_headers = HeaderMap::new();
        paired_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer lmk-lan-paired"),
        );
        paired_headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
        let paired_surface =
            endpoint_request_surface(&endpoint, true, &Method::GET, "/v1/models", &paired_headers);
        assert_eq!(paired_surface, EndpointRequestSurface::M3);
        let paired = endpoint_refusal_response(paired_surface, Some(&policy), &paired_headers);
        assert_eq!(paired.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            paired
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some(origin)
        );
        let paired_body: serde_json::Value =
            serde_json::from_slice(&body_bytes(paired).await).expect("M3 busy JSON");
        assert_eq!(paired_body["error"]["type"], "little_monkey_m3_error");
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
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    fn post_request(path: &str, body: &str) -> ServerRequest {
        ServerRequest {
            method: Method::POST,
            path: path.to_string(),
            query: None,
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

    /// Every route's client, by identity. The interesting half is the third case:
    /// a provider route must not be reachable with the permissive loopback client.
    ///
    /// Asserted by pointer rather than by behaviour because `reqwest::Client`
    /// exposes nothing about its own policy — there is no `client.redirect_policy()`
    /// to read. Behaviour is asserted separately, in the test below.
    #[test]
    fn each_route_is_sent_with_the_client_its_target_class_requires() {
        let deps = test_deps("http://127.0.0.1:11434".to_string());

        for route in [ModelRoute::Llama, ModelRoute::Ollama, ModelRoute::Unknown] {
            assert!(
                std::ptr::eq(client_for(&deps, &route), &deps.local_client),
                "{route:?} reaches only this machine and must use the local client"
            );
        }

        let cloud = ModelRoute::Providers {
            provider_id: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
        };
        assert!(
            std::ptr::eq(client_for(&deps, &cloud), &deps.cloud_client),
            "a provider route carries an API key to a configured host and must use \
             the hardened client"
        );
        // The claim only means something if the two are actually different clients.
        assert!(!std::ptr::eq(&deps.local_client, &deps.cloud_client));
    }

    /// The behavioural half, and the hole the split exists to close: a provider's
    /// `base_url` is user-configured, so a `302` from it must not carry the caller's
    /// API key to whatever host the response named. reqwest strips `Authorization`
    /// across hosts but **not** `x-api-key`, which `providers::add_anthropic_headers`
    /// sets — see `egress::hardened`.
    ///
    /// Driven through `deps.cloud_client` directly rather than through
    /// `handle_chat_completions`, because that path calls `providers::read_key`,
    /// which reads the OS keychain — untestable here, and not what this asserts.
    ///
    /// The counter-assertion is the one that makes this a test of the *split* rather
    /// than of `egress::hardened`: the same redirect, followed by the local client,
    /// proves the two halves really do have different policies and that this file
    /// has not quietly hardened its loopback path.
    #[tokio::test]
    async fn the_cloud_client_refuses_a_redirect_the_local_client_follows() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        /// One-shot listener: answers `answer`, records whether it was reached.
        fn spawn(answer: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = hits.clone();
            std::thread::spawn(move || {
                while let Ok((mut stream, _)) = listener.accept() {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut buffer = [0u8; 1024];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(answer.as_bytes());
                }
            });
            (origin, hits)
        }

        let (target, target_hits) = spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nhi!".to_string(),
        );
        let (entry, entry_hits) = spawn(format!(
            "HTTP/1.1 302 Found\r\nLocation: {target}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));

        let deps = test_deps("http://127.0.0.1:11434".to_string());

        let refused = deps
            .cloud_client
            .get(&entry)
            .header("x-api-key", "sk-do-not-forward-me")
            .send()
            .await;

        // Asserted before the `Err`, because "the target was contacted" names the
        // actual defect where "the request failed" names only a symptom.
        assert_eq!(
            target_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the redirect target must never be contacted by the cloud client"
        );
        assert!(refused.is_err(), "a cross-origin hop must fail the request");
        assert_eq!(entry_hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Counter-test: the local client still follows it. If this ever starts
        // failing, the loopback half has been hardened too, and the reason it must
        // not be is in `ServerDeps::local_client`'s doc.
        let followed = deps
            .local_client
            .get(&entry)
            .send()
            .await
            .expect("the local client keeps reqwest's stock redirect policy");
        assert!(followed.status().is_success());
        assert_eq!(
            target_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the local client is deliberately permissive and must have followed"
        );
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
    fn sha256_hex_is_deterministic() {
        let digest1 = sha256_hex("lmk-abc");
        let digest2 = sha256_hex("lmk-abc");
        let digest3 = sha256_hex("lmk-different");
        assert_eq!(digest1, digest2);
        assert_ne!(digest1, digest3);
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
        refresh_test_model_extensions(&mut deps);
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
    async fn token_scoped_to_local_backend_cannot_probe_or_infer_an_ollama_model() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-local-only",
            "lmk-local-only",
            vec![Scope::Chat],
            vec![Backend::Local],
        )];

        // Exact resolution inspects only the authorized local source. It does
        // not probe Ollama or infer a backend from an unknown string.
        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#),
            "lmk-local-only",
        );
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
        refresh_test_model_extensions(&mut deps);
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
        refresh_test_model_extensions(&mut deps);
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
        refresh_test_model_extensions(&mut deps);
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
        refresh_test_model_extensions(&mut deps);
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
        refresh_test_model_extensions(&mut deps);

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
        refresh_test_model_extensions(&mut deps);

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
        refresh_test_model_extensions(&mut deps);
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
            query: None,
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
            // Exact union resolution first verifies the Ollama identity via
            // `/api/tags`; only then may dispatch reach chat. Keep this unit
            // fake faithful to the real two-request protocol so the SSE pin
            // still tests the relay bytes rather than bypassing resolution.
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let tags = br#"{"models":[{"name":"llama3.1:8b"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    tags.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(tags);
            }
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
        let (_, _, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
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
        let (_, updated, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
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
    fn failed_config_apply_restores_the_previous_port_without_clobbering_live_tokens() {
        let path = temp_config_path();
        let state = AppState::default();
        let mut initial = ApiServerConfig::default();
        initial.port = 4_321;
        save_config_impl(&path, &initial).expect("initial config");
        let view = ApiServerConfigView {
            port: 5_432,
            autostart: true,
            require_token: false,
            expose_ollama: false,
            expose_providers: true,
        };
        let (previous, _, _) =
            set_config_with_state_impl(&state, &path, view).expect("stage config");

        // Simulate a token minted while the async listener reconciliation was
        // attempting its bind. Rollback must restore only runtime fields.
        let mut concurrent = load_config_impl(&path).expect("staged config");
        concurrent.tokens.push(TokenEntry {
            id: "concurrent-token".to_string(),
            label: "Concurrent".to_string(),
            sha256: sha256_hex("lmk-concurrent"),
            scopes: vec![Scope::Models],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
            expires_at: None,
            ..Default::default()
        });
        save_config_impl(&path, &concurrent).expect("concurrent token update");

        restore_config_runtime_fields(&state, &path, &previous).expect("config rollback");
        let restored = load_config_impl(&path).expect("restored config");
        assert_eq!(restored.port, 4_321);
        assert!(!restored.autostart);
        assert!(restored.require_token);
        assert!(restored.expose_ollama);
        assert!(!restored.expose_providers);
        assert_eq!(restored.tokens.len(), 1);
        assert_eq!(restored.tokens[0].id, "concurrent-token");
        let _ = std::fs::remove_file(path);
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

        // Something else holds the port the new config wants — e.g. LM
        // Studio, or the port field was edited to collide with another app.
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let conflicting_port = blocker.local_addr().unwrap().port();

        // The unified reconciler has already joined the old generation before
        // attempting the replacement bind.
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

        drop(first_listener);
        drop(blocker);
    }

    // -------------------------------------------------------------
    // Phase 4: monkey-cli `api-serve` reuse (`provider_catalog_from`,
    // `tokens_from_config`)
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

        // Generous on purpose. This bound exists to turn a hang into a failing
        // test, not to assert how fast a refused bind comes back — a working
        // `run_cli_server` returns its `Err` immediately, so a wide timeout costs
        // a passing run nothing. Two seconds flaked on a loaded windows-latest
        // runner (`Elapsed(())`, on a job that took nine minutes) while develop
        // stayed green on the same code, which is the signature of a deadline
        // measuring the runner rather than the behaviour under test.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
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
            authenticate(&deps, &with_bearer(get_request("/"), &token).headers, 0).is_ok(),
            "the token must authenticate before it's revoked"
        );

        revoke_token_with_state_impl(&state, &path, &entry.id).unwrap();

        deps.tokens = tokens_from_config(&load_config_impl(&path).unwrap());
        assert!(
            authenticate(&deps, &with_bearer(get_request("/"), &token).headers, 0).is_err(),
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
        // paths must all fall through so `handle_request`'s AppHandle-free
        // routes (or the final 404) still get a chance at them.
        assert_eq!(
            extended_route_for(&Method::GET, "/v1/knowledge/query"),
            None
        );
        assert_eq!(extended_route_for(&Method::GET, "/v1/artifacts/"), None);
        assert_eq!(
            extended_route_for(&Method::GET, "/v1/workflows/runs/"),
            None
        );
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
        assert_eq!(
            extended_route_for(&Method::GET, "/v1/local-apps/app-1/run"),
            None
        );
        assert_eq!(
            extended_route_for(&Method::POST, "/v1/local-apps//run"),
            None
        );
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
        deps.tokens = vec![stored_token(
            "tok-chat",
            "lmk-chat-only",
            vec![Scope::Chat],
            vec![Backend::Local],
        )];
        let headers = with_bearer(get_request("/"), "lmk-chat-only").headers;
        assert!(authenticate_local_app_token(&deps, &headers, "app-1").is_err());
    }

    #[test]
    fn authenticate_local_app_token_rejects_missing_or_wrong_bearer_and_ignores_require_token_toggle(
    ) {
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
        let bytes = read_capped_body(
            StreamBody::new(stream),
            1024,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(&bytes[..], b"hello world");
    }

    /// The cap *semantics* now live in `http_policy.rs` and are tested there
    /// once, for both route families. What stays here is the legacy family's
    /// rendering of each rejection, because those bytes (OpenAI envelope + wildcard CORS)
    /// are the client-visible half and differ from the compatibility router's
    /// on purpose.
    #[tokio::test]
    async fn read_capped_body_renders_an_over_cap_body_as_the_legacy_413_envelope() {
        let stream = futures_util::stream::iter(vec![Ok::<_, BoxError>(Frame::data(
            Bytes::from_static(b"0123456789"),
        ))]);
        let response = read_capped_body(
            StreamBody::new(stream),
            4,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        let bytes = body_bytes(response).await;
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"error":{"code":"request_too_large","message":"Request body exceeds the 4-byte limit.","type":"invalid_request_error"}}"#
        );
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
        let response = read_capped_body(
            StreamBody::new(stream),
            1024,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = body_bytes(response).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "body_read_error");
    }

    /// Pins down the socket-lifecycle precondition behind the restart-race fix:
    /// the reconciler must await the unified endpoint task (which resolves only
    /// after its listener is dropped) before rebinding the same port. Cancelling
    /// without joining is insufficient because the task may not have observed
    /// the token yet. This isolates that teardown ordering without needing an
    /// `EndpointHost`, and repeats it to rule out a lucky scheduling order.
    #[tokio::test]
    async fn awaiting_the_endpoint_task_before_rebinding_avoids_the_restart_race() {
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
            let shutdown = tokio_util::sync::CancellationToken::new();
            let shutdown_for_task = shutdown.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_for_task.cancelled() => break,
                        _ = listener.accept() => {}
                    }
                }
                // `listener` dropped here, as in `run_unified_endpoint`.
            });

            // Give the spawned task a chance to actually be polled at least
            // once, so it's genuinely sitting inside `select!` — matching
            // production, where the accept loop has been running for a
            // while before any restart is triggered.
            tokio::task::yield_now().await;

            shutdown.cancel();
            handle.await.unwrap();

            // The rebind itself still has an unavoidable window (closing and
            // reopening a literal port number is inherently a real socket
            // operation, not just an in-process handoff), but it's now the
            // ONLY window in this test, and it's as small as an immediate
            // `.await` — a truly external process would need to win a race
            // measured in microseconds to land in it. A regression in
            // `stop_server_core` itself (the actual thing under test: not
            // awaiting the unified endpoint task before rebinding) would
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
                "rebinding after joining the endpoint task must succeed, not race the old listener's teardown: {rebound:?}"
            );
        }
    }
    /// The admission rule, tested where it actually lives.
    ///
    /// `serve_with_admission` is the test-only default-envelope wrapper around
    /// the production helper, so this is reachable without 65 sockets or an
    /// `EndpointHost`. Host variants may build `ServerDeps` differently, but the
    /// shared connection path must not let them differ on whether a request is
    /// admitted. The CLI's former private path did exactly that, so one helper is
    /// part of the fix rather than a test convenience.
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
            let held = admission
                .try_admit(&shutdown)
                .expect("first request admits");

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
                Response::builder()
                    .status(StatusCode::OK)
                    .body(full_body("ok"))
                    .unwrap()
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

        /// Structural: desktop and CLI have one route call and one connection
        /// authority. Byte-level listener behaviour is pinned separately by
        /// `tests/legacy_route_compatibility.rs`.
        #[test]
        fn no_serving_path_reaches_a_route_without_admission() {
            let source = include_str!("server.rs");
            let production = source
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map(|(before, _)| before)
                .expect("server.rs has a #[cfg(test)] module");

            assert_eq!(
                production.matches("serve_one_request(").count(),
                1,
                "EndpointHost must own the only production route call"
            );
            for authority in ["listener.accept()", "service_fn(", "serve_connection("] {
                assert_eq!(
                    production.matches(authority).count(),
                    1,
                    "desktop and CLI must share one {authority} authority"
                );
            }
            assert_eq!(
                production.matches("serve_with_admission_response(").count(),
                2,
                "one definition and one shared connection-path call"
            );
            assert!(production.contains("async fn run_unified_endpoint("));
            assert!(!production.contains("async fn run_accept_loop("));
            assert!(
                !production.contains("drop(guard)"),
                "the guard must be owned by the response body, not dropped when the \
                 handler returns — see `hold_admission_until_response_ends`"
            );
        }
    }
    /// Cancellation, which the guard carried and nothing read.
    ///
    /// The target is **server shutdown**, not client disconnect. A disconnecting
    /// client is already handled by drop — hyper drops the service future and the
    /// reqwest future with it. Stopping the server was the hole: the former accept
    /// loop spawned connection tasks that nothing joined, so accepted requests
    /// kept streaming after the UI said "stopped". The lifecycle owner now
    /// cancels the endpoint token; `run_unified_endpoint` owns, observes and
    /// drains those tasks before the stop can complete.
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
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream");
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
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
            let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
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

            // What stopping the server does: the lifecycle owner cancels the
            // endpoint token, and every admission token is a child of it.
            server_shutdown.cancel();

            let collected = streaming.into_body().collect().await;
            let error = collected
                .err()
                .expect("a cut stream must not collect cleanly");
            assert!(
                error
                    .to_string()
                    .contains("stopped while this response was streaming"),
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
            let response =
                serve_with_admission(&admission, &server_shutdown, |cancel| async move {
                    *captured.lock().unwrap() = Some(cancel);
                    json_response(StatusCode::OK, json!({"ok": true}))
                })
                .await;
            assert_eq!(response.status(), StatusCode::OK);

            let token = seen
                .lock()
                .unwrap()
                .clone()
                .expect("handler received a token");
            assert!(!token.is_cancelled());
            server_shutdown.cancel();
            assert!(
                token.is_cancelled(),
                "cancelling the server must reach a request's own token"
            );
        }
    }

    /// Pins the two claims the module doc's K8 section makes.
    ///
    /// The first — that neither HTTP route family produces daemon work — is why
    /// the unified listener has no backpressure refusal to make. It is true today by
    /// inspection, which is exactly the kind of fact that rots: a future route
    /// that calls `enqueue` would inherit the obligation silently. So it is
    /// asserted rather than only written down.
    ///
    /// The second is that upstream model traffic goes through the egress meter.
    /// Both are source-level assertions in the style of
    /// `m3_http_server.rs::every_admitted_route_is_guarded_at_the_service_boundary`,
    /// because what is being constrained is which calls exist at all, and no
    /// runtime fixture can observe the absence of a call.
    #[test]
    fn neither_http_route_family_produces_daemon_work_and_both_meter_upstream_bytes() {
        fn production<'a>(source: &'a str, label: &str) -> &'a str {
            source
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map(|(before, _)| before)
                .unwrap_or_else(|| panic!("{label} has a #[cfg(test)] module"))
        }

        let legacy = production(include_str!("server.rs"), "server.rs");
        let managed = production(include_str!("m3_http_server.rs"), "m3_http_server.rs");

        // No route in either family enqueues daemon work, so K8 backpressure has
        // nothing to refuse here. If one of these trips, that route owes the
        // caller a `429` + `Retry-After` built from `backpressure.retry_after_ms`
        // — never the `503`/`server_busy` that bounded admission returns, which is
        // a different condition on a different timescale.
        for (label, source) in [("server.rs", legacy), ("m3_http_server.rs", managed)] {
            // Every way this library can reach the daemon's queue. Not
            // `run_commands::run_submit`: that records a `RunSpec` in the durable
            // ledger for the app's own frontend loop to pick up, which is a
            // different mechanism from the daemon queue and is already forbidden
            // as a route by this module's core invariant. It also appears in the
            // doc comment above, so matching it here would only ever assert that
            // the prose still exists.
            for producer in [
                "daemon_commands::command",
                "queue_client_recipe",
                "queue_mobile_chat_recipe",
            ] {
                assert!(
                    !source.contains(producer),
                    "{label} reached `{producer}`: this route family now produces daemon work \
                     and must honour K8 backpressure in its own error envelope"
                );
            }
        }

        // Upstream model traffic is metered. Both route families proxy raw SSE, so this
        // also pins that the meter — not a buffering wrapper — is what sits on the
        // streaming path; `streamed_upstream_sse_bytes_reach_the_client_unmodified`
        // pins the resulting bytes.
        assert_eq!(
            legacy.matches("crate::egress::send(").count(),
            2,
            "server.rs meters exactly its chat-completions and embeddings proxies"
        );
        assert_eq!(
            managed.matches("crate::egress::send(").count(),
            1,
            "m3_http_server.rs meters its provider-inference proxy"
        );
        for (label, source) in [("server.rs", legacy), ("m3_http_server.rs", managed)] {
            assert!(
                !source.contains(".send()"),
                "{label} has an unmetered upstream request: use `egress::send(builder)`, \
                 or comment why the site cannot be metered"
            );
        }
    }
}
