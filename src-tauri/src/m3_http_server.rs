//! Managed HTTP/SSE listener for the M3 runtime and compatibility hub.
//!
//! This server is deliberately separate from the legacy local reverse proxy
//! in `server.rs`. It binds the exact persisted [`LanServerPolicy`], performs
//! M3 scoped-token authorization for every protected request, and exposes
//! only compatibility inference, model lifecycle, cancellation, and health.
//! It never routes workspace, file, shell, Git, MCP, recipe, or agent tools.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures_util::stream;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::State;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::compatibility_hub::{
    protocol_error_response, rfc3339_from_seconds, translate_embeddings_request,
    translate_ollama_chat_request, translate_request, ApiBackend, ApiScope, CompatibilityError,
    CompatibilityProtocol, LanServerPolicy, ProtocolStreamFrame, TlsPolicy,
};
use crate::m3_commands::M3CommandState;
use crate::m3_runtime_hub::{
    M3ApiCaller, M3ApiDispatchRequest, M3CancelInferenceRequest, M3DeleteModelRequest,
    M3DownloadRequest, M3EmbeddingDispatchRequest, M3ExternalOperationAuthorization, M3HubError,
    M3HubResult, M3InstalledModelView, M3LoadModelRequest, M3OllamaChatDispatchRequest,
    M3OperationContext, M3ProtocolFrameSink, M3RuntimeCapabilityView, M3RuntimeHub, M3RuntimeKind,
    M3UnloadModelRequest,
};

const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_ACTIVE_REQUESTS: usize = 64;
const TLS_KEYCHAIN_SERVICE: &str = "com.littlemonkey.m3.lan-tls";
const MAX_TLS_PEM_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const RUNTIME_HEADER: HeaderName = HeaderName::from_static("x-little-monkey-runtime-id");
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ResponseBody = BoxBody<Bytes, BoxError>;

/// Re-exported from `http_policy` so both listeners share one implementation of
/// the permit/counter/cancellation bookkeeping.
use crate::http_policy::{AdmissionGuard, ServerCounters};

struct ServerInner {
    status: String,
    bind_address: Option<String>,
    port: Option<u16>,
    tls: bool,
    started_at_ms: Option<u64>,
    last_error: Option<String>,
    shutdown: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    counters: Arc<ServerCounters>,
}

impl Default for ServerInner {
    fn default() -> Self {
        Self {
            status: "stopped".to_string(),
            bind_address: None,
            port: None,
            tls: false,
            started_at_ms: None,
            last_error: None,
            shutdown: None,
            task: None,
            counters: Arc::new(ServerCounters::default()),
        }
    }
}

/// Tauri-managed lifecycle state. A dedicated async mutex serializes start,
/// stop, and restart so two UI calls can never publish competing listeners.
pub struct M3HttpServerState {
    lifecycle: tokio::sync::Mutex<()>,
    inner: Mutex<ServerInner>,
}

impl Default for M3HttpServerState {
    fn default() -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            inner: Mutex::new(ServerInner::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct M3HttpServerStatus {
    pub status: String,
    pub bind_address: Option<String>,
    pub port: Option<u16>,
    pub tls: bool,
    pub started_at_ms: Option<u64>,
    pub request_count: u64,
    pub active_requests: usize,
    pub last_request_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTlsIdentity {
    certificate_pem: String,
    private_key_pem: String,
}

fn lock_inner(state: &M3HttpServerState) -> Result<MutexGuard<'_, ServerInner>, String> {
    state
        .inner
        .lock()
        .map_err(|_| "M3 HTTP server state lock is poisoned".to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn snapshot(inner: &ServerInner) -> M3HttpServerStatus {
    let last = inner.counters.last_request_at_ms.load(Ordering::Relaxed);
    M3HttpServerStatus {
        status: inner.status.clone(),
        bind_address: inner.bind_address.clone(),
        port: inner.port,
        tls: inner.tls,
        started_at_ms: inner.started_at_ms,
        request_count: inner.counters.request_count.load(Ordering::Relaxed),
        active_requests: inner.counters.active_requests.load(Ordering::Relaxed),
        last_request_at_ms: (last != 0).then_some(last),
        last_error: inner.last_error.clone(),
    }
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty()
        || reference.len() > 256
        || reference.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        Err("TLS private-key reference must be 1..=256 safe identifier bytes".to_string())
    } else {
        Ok(())
    }
}

fn tls_keychain_entry(reference: &str) -> Result<keyring::Entry, String> {
    validate_reference(reference)?;
    keyring::Entry::new(TLS_KEYCHAIN_SERVICE, reference)
        .map_err(|error| format!("Could not access the M3 TLS keychain entry: {error}"))
}

fn pem_blocks(bytes: &[u8], label: &str) -> Result<Vec<Vec<u8>>, String> {
    if bytes.len() > MAX_TLS_PEM_BYTES {
        return Err(format!(
            "TLS {label} PEM exceeds the {MAX_TLS_PEM_BYTES}-byte limit"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| format!("TLS {label} PEM is not UTF-8"))?;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut rest = text;
    let mut output = Vec::new();
    while let Some(start) = rest.find(&begin) {
        rest = &rest[start + begin.len()..];
        let finish = rest
            .find(&end)
            .ok_or_else(|| format!("TLS PEM block '{label}' is incomplete"))?;
        let encoded = rest[..finish]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| format!("TLS PEM block '{label}' has invalid base64"))?;
        if decoded.is_empty() {
            return Err(format!("TLS PEM block '{label}' is empty"));
        }
        output.push(decoded);
        rest = &rest[finish + end.len()..];
    }
    if output.is_empty() {
        Err(format!("TLS PEM has no '{label}' block"))
    } else {
        Ok(output)
    }
}

fn tls_config_from_identity(
    identity: &StoredTlsIdentity,
    minimum_version: &str,
) -> Result<(rustls::ServerConfig, String), String> {
    let certificate_values = pem_blocks(identity.certificate_pem.as_bytes(), "CERTIFICATE")?;
    let fingerprint = Sha256::digest(&certificate_values[0])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let certificates = certificate_values
        .into_iter()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    let key = if let Ok(values) = pem_blocks(identity.private_key_pem.as_bytes(), "PRIVATE KEY") {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(values[0].clone()))
    } else if let Ok(values) = pem_blocks(identity.private_key_pem.as_bytes(), "RSA PRIVATE KEY") {
        PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(values[0].clone()))
    } else {
        let values = pem_blocks(identity.private_key_pem.as_bytes(), "EC PRIVATE KEY")?;
        PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(values[0].clone()))
    };
    let versions: &[&'static rustls::SupportedProtocolVersion] = match minimum_version {
        "1.3" => &[&rustls::version::TLS13],
        "1.2" => &[&rustls::version::TLS13, &rustls::version::TLS12],
        _ => return Err("Minimum TLS version must be 1.2 or 1.3".to_string()),
    };
    let config = rustls::ServerConfig::builder_with_protocol_versions(versions)
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| {
            format!("TLS certificate and private key do not form an identity: {error}")
        })?;
    Ok((config, fingerprint))
}

fn store_tls_identity_core(
    reference: &str,
    certificate_pem: &str,
    private_key_pem: &str,
) -> Result<String, String> {
    let identity = StoredTlsIdentity {
        certificate_pem: certificate_pem.to_string(),
        private_key_pem: private_key_pem.to_string(),
    };
    let (_, fingerprint) = tls_config_from_identity(&identity, "1.2")?;
    let encoded = serde_json::to_string(&identity)
        .map_err(|error| format!("Could not encode the M3 TLS identity: {error}"))?;
    tls_keychain_entry(reference)?
        .set_password(&encoded)
        .map_err(|error| {
            format!("Could not store the M3 TLS identity in the OS keychain: {error}")
        })?;
    Ok(fingerprint)
}

fn load_tls_identity(reference: &str) -> Result<StoredTlsIdentity, String> {
    let encoded = tls_keychain_entry(reference)?
        .get_password()
        .map_err(|error| {
            format!("Could not read the M3 TLS identity from the OS keychain: {error}")
        })?;
    serde_json::from_str(&encoded)
        .map_err(|_| "The M3 TLS keychain value is not a valid identity bundle".to_string())
}

async fn tls_acceptor(policy: &LanServerPolicy) -> Result<Option<TlsAcceptor>, String> {
    let TlsPolicy::Certificate {
        certificate_sha256,
        private_key_reference,
        minimum_version,
    } = &policy.tls
    else {
        return Ok(None);
    };
    let reference = private_key_reference.clone();
    let minimum = minimum_version.clone();
    let expected = certificate_sha256.to_ascii_lowercase();
    let (config, actual) = tokio::task::spawn_blocking(move || {
        let identity = load_tls_identity(&reference)?;
        tls_config_from_identity(&identity, &minimum)
    })
    .await
    .map_err(|error| format!("M3 TLS identity task failed: {error}"))??;
    if actual != expected {
        return Err(format!(
            "M3 TLS certificate fingerprint mismatch (expected {expected}, got {actual})"
        ));
    }
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}

async fn bind_policy(policy: &LanServerPolicy) -> Result<TcpListener, String> {
    let address = policy
        .bind_address
        .parse::<IpAddr>()
        .map_err(|error| format!("Invalid M3 bind address: {error}"))?;
    TcpListener::bind(SocketAddr::new(address, policy.port))
        .await
        .map_err(|error| {
            crate::http_policy::describe_bind_error(
                crate::http_policy::ListenerRole::CompatibilityListener,
                &policy.bind_address,
                policy.port,
                &error,
            )
        })
}

fn record_start_preflight_error(
    state: &M3HttpServerState,
    policy: &LanServerPolicy,
    error: &str,
) -> Result<(), String> {
    let mut inner = lock_inner(state)?;
    let existing_listener_is_running =
        inner.status == "running" && inner.task.as_ref().is_some_and(|task| !task.is_finished());
    inner.last_error = Some(error.to_string());
    if !existing_listener_is_running {
        inner.status = "error".to_string();
        inner.bind_address = Some(policy.bind_address.clone());
        inner.port = Some(policy.port);
        inner.tls = matches!(policy.tls, TlsPolicy::Certificate { .. });
        inner.started_at_ms = None;
    }
    Ok(())
}

async fn stop_server_locked(state: &M3HttpServerState) -> Result<M3HttpServerStatus, String> {
    let (shutdown, task) = {
        let mut inner = lock_inner(state)?;
        (inner.shutdown.take(), inner.task.take())
    };
    if let Some(shutdown) = shutdown {
        shutdown.cancel();
    }
    if let Some(task) = task {
        let _ = task.await;
    }
    let mut inner = lock_inner(state)?;
    inner.status = "stopped".to_string();
    inner.bind_address = None;
    inner.port = None;
    inner.tls = false;
    inner.started_at_ms = None;
    Ok(snapshot(&inner))
}

/// Starts the persisted M3 policy listener. This core is also the startup
/// hook entrypoint used by `lib.rs`; the Tauri command below is a thin wrapper.
pub async fn start_server_core(
    state: &M3HttpServerState,
    hub: Arc<M3RuntimeHub>,
) -> Result<M3HttpServerStatus, String> {
    let _lifecycle = state.lifecycle.lock().await;
    let policy = hub
        .lan_policy()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Configure an M3 LAN policy before starting its HTTP server".to_string())?;
    M3RuntimeHub::validate_lan_policy(&policy).map_err(|error| error.to_string())?;
    let acceptor = match tls_acceptor(&policy).await {
        Ok(value) => value,
        Err(error) => {
            record_start_preflight_error(state, &policy, &error)?;
            return Err(error);
        }
    };
    let _ = stop_server_locked(state).await?;
    {
        let mut inner = lock_inner(state)?;
        inner.status = "starting".to_string();
        inner.bind_address = Some(policy.bind_address.clone());
        inner.port = Some(policy.port);
        inner.tls = acceptor.is_some();
        inner.started_at_ms = None;
        inner.last_error = None;
    }
    let listener = match bind_policy(&policy).await {
        Ok(listener) => listener,
        Err(error) => {
            let mut inner = lock_inner(state)?;
            inner.status = "error".to_string();
            inner.bind_address = Some(policy.bind_address.clone());
            inner.port = Some(policy.port);
            inner.tls = acceptor.is_some();
            inner.last_error = Some(error.clone());
            return Err(error);
        }
    };
    let shutdown = CancellationToken::new();
    let counters = Arc::new(ServerCounters::default());
    let task = tokio::spawn(run_accept_loop(
        hub,
        listener,
        policy.clone(),
        acceptor.clone(),
        shutdown.clone(),
        counters.clone(),
    ));
    let mut inner = lock_inner(state)?;
    inner.status = "running".to_string();
    inner.bind_address = Some(policy.bind_address);
    inner.port = Some(policy.port);
    inner.tls = acceptor.is_some();
    inner.started_at_ms = Some(now_ms());
    inner.last_error = None;
    inner.shutdown = Some(shutdown);
    inner.task = Some(task);
    inner.counters = counters;
    Ok(snapshot(&inner))
}

pub async fn stop_server_core(state: &M3HttpServerState) -> Result<M3HttpServerStatus, String> {
    let _lifecycle = state.lifecycle.lock().await;
    stop_server_locked(state).await
}

#[tauri::command]
pub async fn m3_http_server_start(
    hub: State<'_, M3CommandState>,
    server: State<'_, M3HttpServerState>,
) -> Result<M3HttpServerStatus, String> {
    start_server_core(&server, hub.hub.clone()).await
}

#[tauri::command]
pub async fn m3_http_server_stop(
    server: State<'_, M3HttpServerState>,
) -> Result<M3HttpServerStatus, String> {
    stop_server_core(&server).await
}

#[tauri::command]
pub fn m3_http_server_status(
    server: State<'_, M3HttpServerState>,
) -> Result<M3HttpServerStatus, String> {
    let inner = lock_inner(&server)?;
    Ok(snapshot(&inner))
}

#[tauri::command]
pub async fn m3_http_server_store_tls_identity(
    reference: String,
    certificate_pem: String,
    private_key_pem: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        store_tls_identity_core(&reference, &certificate_pem, &private_key_pem)
    })
    .await
    .map_err(|error| format!("M3 TLS keychain task failed: {error}"))?
}

/// m3's request guard: the shared [`AdmissionGuard`] plus this listener's own
/// operation contract. The permit, counters and cancellation bookkeeping are the
/// shared implementation; only `context()` is m3-specific.
struct RequestGuard {
    inner: AdmissionGuard,
}

impl RequestGuard {
    fn new(
        counters: Arc<ServerCounters>,
        permit: OwnedSemaphorePermit,
        server_shutdown: &CancellationToken,
    ) -> Self {
        Self {
            inner: AdmissionGuard::new(counters, permit, server_shutdown),
        }
    }

    fn context(&self) -> M3OperationContext {
        M3OperationContext {
            cancellation: self.inner.cancellation(),
            timeout_ms: REQUEST_TIMEOUT_MS,
        }
    }
}

#[derive(Clone)]
enum HttpAuth {
    Internal,
    External {
        bearer_token: String,
        remote_address: String,
    },
}

fn full_body(bytes: impl Into<Bytes>) -> ResponseBody {
    Full::new(bytes.into())
        .map_err(|never: Infallible| match never {})
        .boxed()
}

fn json_response(status: StatusCode, value: Value) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(Bytes::from(value.to_string())))
        .expect("fixed M3 JSON response is valid")
}

fn empty_response(status: StatusCode) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(full_body(Bytes::new()))
        .expect("fixed empty M3 response is valid")
}

fn error_response(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response<ResponseBody> {
    json_response(
        status,
        json!({
            "error": {
                "code": code,
                "message": message.into(),
                "type": "little_monkey_m3_error"
            }
        }),
    )
}

/// Renders a request-translation failure in the CLIENT protocol's own error
/// envelope (OpenAI `{"error":{...}}` vs Anthropic `{"type":"error",...}`)
/// rather than this server's internal `little_monkey_m3_error` shape, so an
/// OpenAI or Anthropic SDK can actually parse what went wrong. Only used
/// where a real [`CompatibilityError`] is still in hand — i.e. the
/// translation boundary. Errors raised after translation (auth, quota,
/// runtime, storage) are about this server, not the protocol, and keep the
/// generic shape via [`hub_error_response`].
fn translation_error_response(
    protocol: CompatibilityProtocol,
    error: &CompatibilityError,
) -> Response<ResponseBody> {
    let (status, body, retry_after_ms) = protocol_error_response(protocol, error);
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    let mut response = json_response(status, body);
    if let Some(milliseconds) = retry_after_ms {
        let seconds = milliseconds.div_ceil(1_000);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

fn hub_error_response(error: M3HubError) -> Response<ResponseBody> {
    let (status, code) = match &error {
        M3HubError::Invalid { .. } | M3HubError::Compatibility(_) | M3HubError::Json(_) => {
            (StatusCode::BAD_REQUEST, "invalid_request")
        }
        M3HubError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
        M3HubError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
        M3HubError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        M3HubError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
        M3HubError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        M3HubError::Cancelled { .. } => (StatusCode::REQUEST_TIMEOUT, "cancelled"),
        M3HubError::Timeout { .. } => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
        M3HubError::Storage { .. } => (StatusCode::INSUFFICIENT_STORAGE, "storage_quota"),
        M3HubError::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
        M3HubError::Integrity { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "integrity_error"),
        M3HubError::Transport(_) | M3HubError::Runtime(_) => {
            (StatusCode::BAD_GATEWAY, "runtime_error")
        }
        M3HubError::State(_) | M3HubError::Io { .. } | M3HubError::LockPoisoned => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    };
    let retry_after = match &error {
        M3HubError::RateLimited { retry_after_ms } => Some((*retry_after_ms).div_ceil(1_000)),
        _ => None,
    };
    let mut response = error_response(status, code, error.to_string());
    if let Some(seconds) = retry_after {
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

fn is_allowed_path(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/v1/models"
            | "/v1/chat/completions"
            | "/v1/responses"
            | "/v1/messages"
            | "/v1/embeddings"
            | "/v1/models/download"
            | "/v1/models/load"
            | "/v1/models/unload"
            | "/v1/models/status"
            | "/v1/models/delete"
            | "/v1/requests/cancel"
            | "/api/tags"
            | "/api/chat"
    )
}

fn origin_allowed(policy: &LanServerPolicy, origin: &str) -> bool {
    policy
        .cors_allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(origin))
}

fn apply_security_headers(
    response: &mut Response<ResponseBody>,
    policy: &LanServerPolicy,
    origin: Option<&str>,
    request_id: Option<&str>,
) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if let Some(origin) = origin.filter(|origin| origin_allowed(policy, origin)) {
        if let Ok(value) = HeaderValue::from_str(origin) {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
            response
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
    if let Some(request_id) = request_id {
        if let Ok(value) = HeaderValue::from_str(request_id) {
            response.headers_mut().insert(REQUEST_ID_HEADER, value);
        }
    }
}

fn cors_preflight(policy: &LanServerPolicy, origin: Option<&str>) -> Response<ResponseBody> {
    let Some(origin) = origin else {
        return error_response(
            StatusCode::FORBIDDEN,
            "cors_denied",
            "CORS preflight requires an Origin header",
        );
    };
    if !origin_allowed(policy, origin) {
        return error_response(
            StatusCode::FORBIDDEN,
            "cors_denied",
            "Origin is not in the M3 CORS allowlist",
        );
    }
    let mut response = empty_response(StatusCode::NO_CONTENT);
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "authorization, content-type, x-request-id, x-little-monkey-runtime-id",
        ),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(char::is_whitespace)?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn request_auth(
    headers: &HeaderMap,
    policy: &LanServerPolicy,
    remote_address: IpAddr,
) -> Result<HttpAuth, Response<ResponseBody>> {
    match bearer_token(headers) {
        Some(bearer_token) => Ok(HttpAuth::External {
            bearer_token,
            remote_address: remote_address.to_string(),
        }),
        None if !policy.require_authentication && policy.is_loopback() => Ok(HttpAuth::Internal),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing_bearer_token",
            "A paired M3 bearer token is required",
        )),
    }
}

fn operation_authorization(
    auth: &HttpAuth,
    scope: ApiScope,
    backend: ApiBackend,
    model_id: Option<String>,
    input_bytes: u64,
    destructive_confirmation: Option<String>,
) -> Option<M3ExternalOperationAuthorization> {
    let HttpAuth::External {
        bearer_token,
        remote_address,
    } = auth
    else {
        return None;
    };
    Some(M3ExternalOperationAuthorization {
        bearer_token: bearer_token.clone(),
        scope,
        backend,
        model_id,
        input_bytes,
        remote_address: remote_address.clone(),
        destructive_confirmation,
        now_ms: now_ms(),
    })
}

fn authorize_operation(
    hub: &M3RuntimeHub,
    auth: &HttpAuth,
    scope: ApiScope,
    backend: ApiBackend,
    model_id: Option<String>,
    input_bytes: u64,
    destructive_confirmation: Option<String>,
) -> M3HubResult<()> {
    if let Some(request) = operation_authorization(
        auth,
        scope,
        backend,
        model_id,
        input_bytes,
        destructive_confirmation,
    ) {
        hub.authorize_external_operation(&request)?;
    }
    Ok(())
}

fn protocol_scope(protocol: CompatibilityProtocol) -> ApiScope {
    match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => ApiScope::ChatCompletions,
        CompatibilityProtocol::OpenAiResponses => ApiScope::Responses,
        CompatibilityProtocol::AnthropicMessages => ApiScope::Messages,
    }
}

fn backend_for_kind(kind: M3RuntimeKind) -> ApiBackend {
    match kind {
        M3RuntimeKind::Ollama => ApiBackend::Ollama,
        M3RuntimeKind::LlamaCpp => ApiBackend::ManagedLocal,
        M3RuntimeKind::Mlx => ApiBackend::Mlx,
    }
}

fn runtime_by_id(
    runtimes: &[M3RuntimeCapabilityView],
    runtime_id: &str,
) -> M3HubResult<M3RuntimeCapabilityView> {
    runtimes
        .iter()
        .find(|runtime| runtime.descriptor.runtime_id == runtime_id)
        .cloned()
        .ok_or_else(|| M3HubError::NotFound(format!("runtime {runtime_id}")))
}

fn runtime_by_hub_id(hub: &M3RuntimeHub, runtime_id: &str) -> M3HubResult<M3RuntimeCapabilityView> {
    let runtimes = hub.list_runtimes()?;
    runtime_by_id(&runtimes, runtime_id)
}

fn runtime_for_model(
    hub: &M3RuntimeHub,
    headers: &HeaderMap,
    model_id: &str,
) -> M3HubResult<M3RuntimeCapabilityView> {
    let runtimes = hub.list_runtimes()?;
    if let Some(runtime_id) = headers
        .get(&RUNTIME_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        return runtime_by_id(&runtimes, runtime_id);
    }
    let installed = hub.list_installed_models()?;
    let kind = installed
        .iter()
        .find(|model| model.model_id == model_id)
        .map(|model| model.runtime)
        .ok_or_else(|| {
            M3HubError::NotFound(format!(
                "model {model_id}; unmanaged runtime models require the x-little-monkey-runtime-id header"
            ))
        })?;
    runtimes
        .into_iter()
        .find(|runtime| runtime.can_infer && runtime.descriptor.kind == kind)
        .ok_or_else(|| M3HubError::NotFound(format!("inference runtime for model {model_id}")))
}

fn installed_by_asset(hub: &M3RuntimeHub, asset_id: &str) -> M3HubResult<M3InstalledModelView> {
    hub.list_installed_models()?
        .into_iter()
        .find(|model| model.asset_id == asset_id)
        .ok_or_else(|| M3HubError::NotFound(format!("installed model asset {asset_id}")))
}

async fn read_capped_body<B>(mut body: B, limit: usize) -> Result<Bytes, Response<ResponseBody>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut output = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if output.len().saturating_add(data.len()) > limit {
                        return Err(error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "request_too_large",
                            format!("Request body exceeds the {limit}-byte limit"),
                        ));
                    }
                    output.extend_from_slice(data);
                }
            }
            Some(Err(_)) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "body_read_error",
                    "The request body could not be read completely",
                ))
            }
            None => break,
        }
    }
    Ok(Bytes::from(output))
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, Response<ResponseBody>> {
    serde_json::from_slice(body).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("Request JSON does not match the endpoint schema: {error}"),
        )
    })
}

struct ChannelFrameSink {
    sender: mpsc::Sender<Bytes>,
}

impl M3ProtocolFrameSink for ChannelFrameSink {
    fn emit(&mut self, frame: ProtocolStreamFrame) -> Result<(), String> {
        self.sender
            .try_send(Bytes::from(frame.to_sse_bytes()))
            .map_err(|error| format!("HTTP SSE client is disconnected or too slow: {error}"))
    }
}

fn sse_body(receiver: mpsc::Receiver<Bytes>, guard: RequestGuard) -> ResponseBody {
    let stream = stream::unfold((receiver, guard), |(mut receiver, guard)| async move {
        receiver.recv().await.map(|bytes| {
            (
                Ok::<Frame<Bytes>, Infallible>(Frame::data(bytes)),
                (receiver, guard),
            )
        })
    });
    StreamBody::new(stream)
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed()
}

fn sse_response(receiver: mpsc::Receiver<Bytes>, guard: RequestGuard) -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONNECTION, "keep-alive")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(sse_body(receiver, guard))
        .expect("fixed M3 SSE response is valid")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeStatusRequest {
    runtime_id: String,
    #[serde(default)]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpCancelRequest {
    protocol: CompatibilityProtocol,
    runtime_id: String,
    request_id: String,
    model_id: String,
}

async fn discover_models(
    hub: &M3RuntimeHub,
    headers: &HeaderMap,
    auth: &HttpAuth,
    input_bytes: u64,
    context: &M3OperationContext,
) -> M3HubResult<(Value, Option<String>)> {
    let runtimes = hub.list_runtimes()?;
    let selected = if let Some(runtime_id) = headers
        .get(&RUNTIME_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        let runtime = runtime_by_id(&runtimes, runtime_id)?;
        authorize_operation(
            hub,
            auth,
            ApiScope::ModelDiscover,
            runtime.descriptor.api_backend,
            None,
            input_bytes,
            None,
        )?;
        vec![runtime]
    } else if matches!(auth, HttpAuth::Internal) {
        runtimes
    } else {
        let mut selected = None;
        let mut last_error = None;
        for runtime in runtimes {
            match authorize_operation(
                hub,
                auth,
                ApiScope::ModelDiscover,
                runtime.descriptor.api_backend,
                None,
                input_bytes,
                None,
            ) {
                Ok(()) => {
                    selected = Some(runtime);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        vec![selected.ok_or_else(|| {
            last_error.unwrap_or_else(|| M3HubError::NotFound("authorized runtime".to_string()))
        })?]
    };

    let installed = hub.list_installed_models()?;
    let mut models = BTreeMap::<String, Value>::new();
    for runtime in &selected {
        for model in installed
            .iter()
            .filter(|model| model.runtime == runtime.descriptor.kind)
        {
            let active = model.versions.iter().find(|version| version.active);
            models.insert(
                model.model_id.clone(),
                json!({
                    "id": model.model_id,
                    "object": "model",
                    "created": active.map(|version| version.installed_at_ms / 1_000).unwrap_or(0),
                    "owned_by": "little-monkey",
                    "runtime_id": runtime.descriptor.runtime_id,
                    "backend": runtime.descriptor.api_backend,
                    "asset_id": model.asset_id,
                    "size_bytes": active.map(|version| version.size_bytes).unwrap_or(0),
                    "capabilities": model.capabilities,
                }),
            );
        }
        if let Ok(inventory) = hub
            .runtime_inventory(&runtime.descriptor.runtime_id, context)
            .await
        {
            for model in inventory.models {
                models.entry(model.model_id.clone()).or_insert_with(|| {
                    json!({
                        "id": model.model_id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "runtime",
                        "runtime_id": runtime.descriptor.runtime_id,
                        "backend": runtime.descriptor.api_backend,
                        "size_bytes": model.size_bytes,
                        "capabilities": model.capabilities,
                    })
                });
            }
        }
    }
    let selected_runtime = (selected.len() == 1).then(|| selected[0].descriptor.runtime_id.clone());
    Ok((
        json!({ "object": "list", "data": models.into_values().collect::<Vec<_>>() }),
        selected_runtime,
    ))
}

async fn inference_response(
    hub: Arc<M3RuntimeHub>,
    protocol: CompatibilityProtocol,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    guard: &mut Option<RequestGuard>,
) -> Response<ResponseBody> {
    let canonical = match translate_request(protocol, &request_id, &body) {
        Ok(request) => request,
        Err(error) => return translation_error_response(protocol, &error),
    };
    let runtime = match runtime_for_model(&hub, headers, &canonical.model) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    if let Err(error) = authorize_operation(
        &hub,
        auth,
        protocol_scope(protocol),
        runtime.descriptor.api_backend,
        Some(canonical.model.clone()),
        body.len() as u64,
        None,
    ) {
        return hub_error_response(error);
    }
    let request = M3ApiDispatchRequest {
        protocol,
        runtime_id: runtime.descriptor.runtime_id,
        request_id,
        body: body.to_vec(),
        // The HTTP boundary already performed the exact external
        // authorization above. Internal here prevents a second quota debit.
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    let context = guard
        .as_ref()
        .expect("request guard exists before response publication")
        .context();
    if !canonical.stream {
        return match hub.dispatch_api(&request, &context).await {
            Ok(result) => {
                let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK);
                json_response(status, result.body)
            }
            Err(error) => hub_error_response(error),
        };
    }

    // A bounded channel is a deliberate slow-client memory quota. Drivers
    // fail the sink rather than buffering an unbounded SSE transcript.
    let (sender, receiver) = mpsc::channel(64);
    let mut sink = ChannelFrameSink {
        sender: sender.clone(),
    };
    tokio::spawn(async move {
        if let Err(error) = hub.dispatch_api_stream(&request, &mut sink, &context).await {
            let frame = ProtocolStreamFrame {
                event: Some("error".to_string()),
                data: json!({
                    "error": {
                        "code": "stream_error",
                        "message": error.to_string(),
                        "type": "little_monkey_m3_error"
                    }
                })
                .to_string(),
            };
            let _ = sender.try_send(Bytes::from(frame.to_sse_bytes()));
        }
    });
    sse_response(
        receiver,
        guard
            .take()
            .expect("SSE response takes ownership of request guard"),
    )
}

/// Handles `POST /v1/embeddings`, applying the exact same model-resolution,
/// authorization, and error-shape conventions as [`inference_response`].
/// Never fabricates a vector: [`M3RuntimeHub::dispatch_embeddings`] rejects
/// with a clear `unsupported` error when the resolved model's runtime does
/// not genuinely reach an embeddings-capable backend.
async fn embeddings_response(
    hub: Arc<M3RuntimeHub>,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    // `/v1/embeddings` is an OpenAI-shaped route, so a translation failure
    // uses the OpenAI error envelope (there is no dedicated embeddings
    // protocol variant — the envelope is identical across OpenAI routes).
    let canonical = match translate_embeddings_request(&request_id, &body) {
        Ok(request) => request,
        Err(error) => {
            return translation_error_response(
                CompatibilityProtocol::OpenAiChatCompletions,
                &error,
            )
        }
    };
    let runtime = match runtime_for_model(&hub, headers, &canonical.model) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    if let Err(error) = authorize_operation(
        &hub,
        auth,
        ApiScope::Embeddings,
        runtime.descriptor.api_backend,
        Some(canonical.model.clone()),
        body.len() as u64,
        None,
    ) {
        return hub_error_response(error);
    }
    let request = M3EmbeddingDispatchRequest {
        runtime_id: runtime.descriptor.runtime_id,
        request_id,
        body: body.to_vec(),
        // The HTTP boundary already performed the exact external
        // authorization above. Internal here prevents a second quota debit.
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    match hub.dispatch_embeddings(&request, context).await {
        Ok(result) => {
            let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK);
            json_response(status, result.body)
        }
        Err(error) => hub_error_response(error),
    }
}

/// Handles the Ollama-native `POST /api/chat`, reusing the `ChatCompletions`
/// scope (same operation as `/v1/chat/completions`, different wire format —
/// not a new, less-guarded route class) and the same model-resolution and
/// authorization conventions as [`inference_response`]. Always calls the
/// backend non-streaming; see [`M3RuntimeHub::dispatch_ollama_chat`]'s doc
/// for the documented streaming limitation.
async fn ollama_chat_response(
    hub: Arc<M3RuntimeHub>,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let (canonical, _stream_requested) = match translate_ollama_chat_request(&request_id, &body) {
        Ok(value) => value,
        Err(error) => return hub_error_response(M3HubError::from(error)),
    };
    let runtime = match runtime_for_model(&hub, headers, &canonical.model) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    if let Err(error) = authorize_operation(
        &hub,
        auth,
        ApiScope::ChatCompletions,
        runtime.descriptor.api_backend,
        Some(canonical.model.clone()),
        body.len() as u64,
        None,
    ) {
        return hub_error_response(error);
    }
    let request = M3OllamaChatDispatchRequest {
        runtime_id: runtime.descriptor.runtime_id,
        request_id,
        body: body.to_vec(),
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    match hub.dispatch_ollama_chat(&request, context).await {
        Ok(result) => {
            // Ollama's own server picks `application/x-ndjson` for a
            // streamed response and `application/json` for `stream:false`.
            // Both bodies here are the identical single compact-JSON line —
            // see the module doc on `/api/chat`'s streaming limitation.
            let content_type = if result.stream_requested {
                "application/x-ndjson"
            } else {
                "application/json"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(full_body(Bytes::from(format!("{}\n", result.body))))
                .expect("fixed ollama chat response is valid")
        }
        Err(error) => hub_error_response(error),
    }
}

/// Handles the Ollama-native `GET /api/tags`, reshaping the same installed
/// model + reconciled runtime inventory data already used by `/v1/models`
/// into Ollama's own response shape.
async fn ollama_tags_response(
    hub: &M3RuntimeHub,
    headers: &HeaderMap,
    auth: &HttpAuth,
    input_bytes: u64,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    match discover_models(hub, headers, auth, input_bytes, context).await {
        Ok((value, _selected_runtime)) => {
            json_response(StatusCode::OK, ollama_tags_from_models(&value))
        }
        Err(error) => hub_error_response(error),
    }
}

fn ollama_tags_from_models(list: &Value) -> Value {
    let models = list
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries = models
        .iter()
        .map(|model| {
            let name = model
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let size = model.get("size_bytes").and_then(Value::as_u64).unwrap_or(0);
            let created = model.get("created").and_then(Value::as_u64).unwrap_or(0);
            let digest = model
                .get("asset_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            json!({
                "name": name,
                "model": name,
                "modified_at": rfc3339_from_seconds(created),
                "size": size,
                "digest": digest,
                "details": {
                    "parent_model": "",
                    "format": "gguf",
                    "family": "",
                    "families": Value::Null,
                    "parameter_size": "",
                    "quantization_level": "",
                }
            })
        })
        .collect::<Vec<_>>();
    json!({ "models": entries })
}

async fn lifecycle_response(
    hub: &M3RuntimeHub,
    path: &str,
    auth: &HttpAuth,
    body: &Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    match path {
        "/v1/models/download" => {
            let request = match parse_json::<M3DownloadRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            if let Err(error) = authorize_operation(
                hub,
                auth,
                ApiScope::ModelDownload,
                backend_for_kind(request.model.runtime),
                Some(request.model.model_id.clone()),
                body.len() as u64,
                None,
            ) {
                return hub_error_response(error);
            }
            match hub.download_model(&request, context).await {
                Ok(model) => json_response(StatusCode::CREATED, json!(model)),
                Err(error) => hub_error_response(error),
            }
        }
        "/v1/models/load" => {
            let request = match parse_json::<M3LoadModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let model = match installed_by_asset(hub, &request.asset_id) {
                Ok(model) => model,
                Err(error) => return hub_error_response(error),
            };
            let runtime = match runtime_by_hub_id(hub, &request.runtime_id) {
                Ok(runtime) => runtime,
                Err(error) => return hub_error_response(error),
            };
            if let Err(error) = authorize_operation(
                hub,
                auth,
                ApiScope::ModelLoad,
                runtime.descriptor.api_backend,
                Some(model.model_id),
                body.len() as u64,
                None,
            ) {
                return hub_error_response(error);
            }
            match hub.load_model(&request, context).await {
                Ok(()) => json_response(StatusCode::OK, json!({ "loaded": true })),
                Err(error) => hub_error_response(error),
            }
        }
        "/v1/models/unload" => {
            let request = match parse_json::<M3UnloadModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let runtime = match runtime_by_hub_id(hub, &request.runtime_id) {
                Ok(runtime) => runtime,
                Err(error) => return hub_error_response(error),
            };
            if let Err(error) = authorize_operation(
                hub,
                auth,
                ApiScope::ModelUnload,
                runtime.descriptor.api_backend,
                Some(request.model_id.clone()),
                body.len() as u64,
                None,
            ) {
                return hub_error_response(error);
            }
            match hub.unload_model(&request, context).await {
                Ok(()) => json_response(StatusCode::OK, json!({ "unloaded": true })),
                Err(error) => hub_error_response(error),
            }
        }
        "/v1/models/status" => {
            let request = match parse_json::<RuntimeStatusRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let runtime = match runtime_by_hub_id(hub, &request.runtime_id) {
                Ok(runtime) => runtime,
                Err(error) => return hub_error_response(error),
            };
            if let Err(error) = authorize_operation(
                hub,
                auth,
                ApiScope::ModelStatus,
                runtime.descriptor.api_backend,
                request.model_id,
                body.len() as u64,
                None,
            ) {
                return hub_error_response(error);
            }
            let status = hub.runtime_status(&request.runtime_id, context).await;
            let inventory = hub.runtime_inventory(&request.runtime_id, context).await;
            match (status, inventory) {
                (Ok(status), Ok(inventory)) => json_response(
                    StatusCode::OK,
                    json!({ "runtimeId": request.runtime_id, "status": status, "inventory": inventory }),
                ),
                (Err(error), _) | (_, Err(error)) => hub_error_response(error),
            }
        }
        "/v1/models/delete" => {
            let request = match parse_json::<M3DeleteModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let model = match installed_by_asset(hub, &request.asset_id) {
                Ok(model) => model,
                Err(error) => return hub_error_response(error),
            };
            let destructive = (request.confirmation == format!("DELETE {}", request.asset_id))
                .then(|| format!("DELETE {}", model.model_id));
            if let Err(error) = authorize_operation(
                hub,
                auth,
                ApiScope::ModelDelete,
                backend_for_kind(model.runtime),
                Some(model.model_id),
                body.len() as u64,
                destructive,
            ) {
                return hub_error_response(error);
            }
            match hub.delete_model(&request, context).await {
                Ok(deleted) => json_response(StatusCode::OK, json!({ "deleted": deleted })),
                Err(error) => hub_error_response(error),
            }
        }
        _ => error_response(StatusCode::NOT_FOUND, "not_found", "Route does not exist"),
    }
}

async fn cancel_response(
    hub: &M3RuntimeHub,
    auth: &HttpAuth,
    body: &Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let request = match parse_json::<HttpCancelRequest>(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let runtime = match runtime_by_hub_id(hub, &request.runtime_id) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    if let Err(error) = authorize_operation(
        hub,
        auth,
        protocol_scope(request.protocol),
        runtime.descriptor.api_backend,
        Some(request.model_id.clone()),
        body.len() as u64,
        None,
    ) {
        return hub_error_response(error);
    }
    let request = M3CancelInferenceRequest {
        protocol: request.protocol,
        runtime_id: request.runtime_id,
        request_id: request.request_id,
        model_id: request.model_id,
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    match hub.cancel_inference(&request, context).await {
        Ok(cancelled) => json_response(StatusCode::OK, json!({ "cancelled": cancelled })),
        Err(error) => hub_error_response(error),
    }
}

async fn handle_http_request(
    hub: Arc<M3RuntimeHub>,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    request: Request<Incoming>,
    mut guard: Option<RequestGuard>,
) -> Response<ResponseBody> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let headers = request.headers().clone();
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let request_id = headers
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .map(str::to_string)
        .unwrap_or_else(|| format!("http-{}", Uuid::new_v4().simple()));

    let mut response = if !is_allowed_path(&path) {
        error_response(StatusCode::NOT_FOUND, "not_found", "Route does not exist")
    } else if origin
        .as_deref()
        .is_some_and(|value| !origin_allowed(&policy, value))
    {
        error_response(
            StatusCode::FORBIDDEN,
            "cors_denied",
            "Origin is not in the M3 CORS allowlist",
        )
    } else if method == Method::OPTIONS {
        cors_preflight(&policy, origin.as_deref())
    } else if method == Method::GET && path == "/health" {
        json_response(
            StatusCode::OK,
            json!({
                "status": "ok",
                "service": "little-monkey-m3",
                "schemaVersion": 1,
                "tls": matches!(policy.tls, TlsPolicy::Certificate { .. }),
                "conformance": hub.conformance_manifest(),
            }),
        )
    } else {
        let auth = match request_auth(&headers, &policy, remote_address) {
            Ok(auth) => auth,
            Err(response) => {
                let mut response = response;
                apply_security_headers(
                    &mut response,
                    &policy,
                    origin.as_deref(),
                    Some(&request_id),
                );
                return response;
            }
        };
        let body = match read_capped_body(request.into_body(), MAX_REQUEST_BODY_BYTES).await {
            Ok(body) => body,
            Err(response) => {
                let mut response = response;
                apply_security_headers(
                    &mut response,
                    &policy,
                    origin.as_deref(),
                    Some(&request_id),
                );
                return response;
            }
        };
        let context = guard
            .as_ref()
            .expect("protected M3 route has an active request guard")
            .context();
        match (method, path.as_str()) {
            (Method::GET, "/v1/models") => {
                match discover_models(&hub, &headers, &auth, body.len() as u64, &context).await {
                    Ok((value, selected_runtime)) => {
                        let mut response = json_response(StatusCode::OK, value);
                        if let Some(runtime) = selected_runtime {
                            if let Ok(value) = HeaderValue::from_str(&runtime) {
                                response.headers_mut().insert(RUNTIME_HEADER.clone(), value);
                            }
                        }
                        response
                    }
                    Err(error) => hub_error_response(error),
                }
            }
            (Method::POST, "/v1/chat/completions") => {
                inference_response(
                    hub.clone(),
                    CompatibilityProtocol::OpenAiChatCompletions,
                    &headers,
                    request_id.clone(),
                    &auth,
                    body,
                    &mut guard,
                )
                .await
            }
            (Method::POST, "/v1/responses") => {
                inference_response(
                    hub.clone(),
                    CompatibilityProtocol::OpenAiResponses,
                    &headers,
                    request_id.clone(),
                    &auth,
                    body,
                    &mut guard,
                )
                .await
            }
            (Method::POST, "/v1/messages") => {
                inference_response(
                    hub.clone(),
                    CompatibilityProtocol::AnthropicMessages,
                    &headers,
                    request_id.clone(),
                    &auth,
                    body,
                    &mut guard,
                )
                .await
            }
            (Method::POST, "/v1/embeddings") => {
                embeddings_response(hub.clone(), &headers, request_id.clone(), &auth, body, &context)
                    .await
            }
            (Method::GET, "/api/tags") => {
                ollama_tags_response(&hub, &headers, &auth, body.len() as u64, &context).await
            }
            (Method::POST, "/api/chat") => {
                ollama_chat_response(hub.clone(), &headers, request_id.clone(), &auth, body, &context)
                    .await
            }
            (
                Method::POST,
                "/v1/models/download"
                | "/v1/models/load"
                | "/v1/models/unload"
                | "/v1/models/status"
                | "/v1/models/delete",
            ) => lifecycle_response(&hub, &path, &auth, &body, &context).await,
            (Method::POST, "/v1/requests/cancel") => {
                cancel_response(&hub, &auth, &body, &context).await
            }
            _ => {
                let mut response = error_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "This M3 route does not support the requested method",
                );
                response.headers_mut().insert(
                    header::ALLOW,
                    HeaderValue::from_static("GET, POST, OPTIONS"),
                );
                response
            }
        }
    };
    apply_security_headers(&mut response, &policy, origin.as_deref(), Some(&request_id));
    response
}

async fn serve_connection<I>(
    io: I,
    hub: Arc<M3RuntimeHub>,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    request_limit: Arc<Semaphore>,
    counters: Arc<ServerCounters>,
    shutdown: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection_shutdown = shutdown.clone();
    let service = service_fn(move |request: Request<Incoming>| {
        let hub = hub.clone();
        let policy = policy.clone();
        let request_limit = request_limit.clone();
        let counters = counters.clone();
        let shutdown = shutdown.clone();
        async move {
            let response = match request_limit.try_acquire_owned() {
                Ok(permit) => {
                    let guard = RequestGuard::new(counters, permit, &shutdown);
                    handle_http_request(hub, policy, remote_address, request, Some(guard)).await
                }
                Err(_) => {
                    let origin = request
                        .headers()
                        .get(header::ORIGIN)
                        .and_then(|value| value.to_str().ok());
                    let request_id = request
                        .headers()
                        .get(&REQUEST_ID_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 256
                                && !value.chars().any(char::is_control)
                        })
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("http-{}", Uuid::new_v4().simple()));
                    let mut response = error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "server_busy",
                        "The M3 server active-request quota is exhausted",
                    );
                    apply_security_headers(&mut response, &policy, origin, Some(&request_id));
                    response
                }
            };
            Ok::<_, Infallible>(response)
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

async fn serve_plain_connection(
    stream: TcpStream,
    hub: Arc<M3RuntimeHub>,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    request_limit: Arc<Semaphore>,
    counters: Arc<ServerCounters>,
    shutdown: CancellationToken,
) {
    serve_connection(
        stream,
        hub,
        policy,
        remote_address,
        request_limit,
        counters,
        shutdown,
    )
    .await;
}

async fn serve_tls_connection(
    stream: TlsStream<TcpStream>,
    hub: Arc<M3RuntimeHub>,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    request_limit: Arc<Semaphore>,
    counters: Arc<ServerCounters>,
    shutdown: CancellationToken,
) {
    serve_connection(
        stream,
        hub,
        policy,
        remote_address,
        request_limit,
        counters,
        shutdown,
    )
    .await;
}

async fn run_accept_loop(
    hub: Arc<M3RuntimeHub>,
    listener: TcpListener,
    policy: LanServerPolicy,
    acceptor: Option<TlsAcceptor>,
    shutdown: CancellationToken,
    counters: Arc<ServerCounters>,
) {
    let policy = Arc::new(policy);
    let request_limit = Arc::new(Semaphore::new(MAX_ACTIVE_REQUESTS));
    let connection_limit = Arc::new(Semaphore::new(MAX_ACTIVE_REQUESTS * 2));
    loop {
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, remote) = match accepted {
            Ok(value) => value,
            Err(_) => continue,
        };
        let connection_permit = match connection_limit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => continue,
        };
        let hub = hub.clone();
        let policy = policy.clone();
        let request_limit = request_limit.clone();
        let counters = counters.clone();
        let shutdown_for_connection = shutdown.clone();
        if let Some(acceptor) = acceptor.clone() {
            tokio::spawn(async move {
                let _connection_permit = connection_permit;
                let accepted = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    acceptor.accept(stream),
                )
                .await;
                if let Ok(Ok(stream)) = accepted {
                    serve_tls_connection(
                        stream,
                        hub,
                        policy,
                        remote.ip(),
                        request_limit,
                        counters,
                        shutdown_for_connection,
                    )
                    .await;
                }
            });
        } else {
            tokio::spawn(async move {
                let _connection_permit = connection_permit;
                serve_plain_connection(
                    stream,
                    hub,
                    policy,
                    remote.ip(),
                    request_limit,
                    counters,
                    shutdown_for_connection,
                )
                .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::compatibility_hub::{LanStateProtector, OsLanEntropy};
    use crate::m3_runtime_hub::{
        DefaultM3LanAccessFactory, M3DownloadTransport, M3HardwareProbe, M3HubConfig,
        M3RuntimeHubDependencies, ReqwestM3DownloadTransport, SystemM3Clock,
    };
    use crate::runtime_adapter::{
        AcceleratorCapability, AcceleratorKind, HardwareSnapshot, PlatformCapabilities,
    };

    struct TestHardware;

    impl M3HardwareProbe for TestHardware {
        fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
            Ok(HardwareSnapshot {
                captured_at_ms: now_ms(),
                total_ram_bytes: 16 * 1024 * 1024 * 1024,
                available_ram_bytes: 12 * 1024 * 1024 * 1024,
                logical_cpu_count: 8,
                platform: PlatformCapabilities::current(vec![AcceleratorCapability {
                    kind: AcceleratorKind::Cpu,
                    available: true,
                    device_names: vec!["test-cpu".to_string()],
                    total_memory_bytes: None,
                    available_memory_bytes: None,
                }]),
            })
        }
    }

    struct TestProtector;

    impl LanStateProtector for TestProtector {
        fn protector_id(&self) -> &str {
            "m3-http-test-protector"
        }

        fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
            let mut hash = Sha256::new();
            hash.update(b"m3-http-test-key");
            hash.update(canonical_state);
            Ok(hash.finalize().to_vec())
        }

        fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
            let expected = self.authenticate(canonical_state)?;
            if expected == tag {
                Ok(())
            } else {
                Err("test state authentication failed".to_string())
            }
        }
    }

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "little-monkey-m3-http-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).expect("create M3 HTTP test root");
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_hub(root: &TestRoot) -> Arc<M3RuntimeHub> {
        let download: Arc<dyn M3DownloadTransport> =
            Arc::new(ReqwestM3DownloadTransport::new().expect("download transport"));
        Arc::new(
            M3RuntimeHub::new(
                &root.0,
                M3HubConfig {
                    storage_quota_bytes: 8 * 1024 * 1024 * 1024,
                    storage_reserve_bytes: 1024 * 1024 * 1024,
                    ..M3HubConfig::default()
                },
                M3RuntimeHubDependencies {
                    clock: Arc::new(SystemM3Clock),
                    hardware: Arc::new(TestHardware),
                    download,
                    catalogs: Vec::new(),
                    runtimes: Vec::new(),
                    runtime_reconciler: None,
                    lan_factory: Some(Arc::new(DefaultM3LanAccessFactory::new(
                        Arc::new(OsLanEntropy),
                        Arc::new(TestProtector),
                    ))),
                },
            )
            .expect("M3 HTTP test hub"),
        )
    }

    async fn free_loopback_port() -> Option<u16> {
        match TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => Some(listener.local_addr().expect("ephemeral address").port()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping loopback assertion: sandbox forbids local listeners");
                None
            }
            Err(error) => panic!("bind ephemeral test port: {error}"),
        }
    }

    #[test]
    fn route_allowlist_never_exposes_agent_or_workspace_tools() {
        for allowed in [
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1/embeddings",
            "/v1/models/download",
            "/v1/models/load",
            "/v1/models/unload",
            "/v1/models/status",
            "/v1/models/delete",
            "/v1/requests/cancel",
            "/api/tags",
            "/api/chat",
        ] {
            assert!(is_allowed_path(allowed), "missing allowed route {allowed}");
        }
        for forbidden in [
            "/v1/tools",
            "/v1/tool_run_shell",
            "/v1/files",
            "/v1/git",
            "/v1/mcp",
            "/v1/workspace",
            "/v1/recipes",
        ] {
            assert!(
                !is_allowed_path(forbidden),
                "forbidden route leaked: {forbidden}"
            );
        }
    }

    #[test]
    fn cors_is_exact_and_never_accepts_wildcards_or_near_matches() {
        let mut policy = LanServerPolicy::default();
        policy.cors_allowlist = vec!["http://localhost:5173".to_string()];
        assert!(origin_allowed(&policy, "http://localhost:5173"));
        assert!(!origin_allowed(&policy, "http://localhost:51730"));
        assert!(!origin_allowed(&policy, "https://attacker.example"));
        assert!(!origin_allowed(&policy, "*"));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_but_never_accepts_empty_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bEaReR paired-token"),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("paired-token"));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer    "),
        );
        assert_eq!(bearer_token(&headers), None);
    }

    #[tokio::test]
    async fn request_body_limit_rejects_before_buffering_past_the_quota() {
        let body = Full::new(Bytes::from_static(b"12345"));
        let response = read_capped_body(body, 4)
            .await
            .expect_err("oversized request must fail");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn bind_conflicts_surface_without_panicking() {
        let first = match TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping bind-conflict assertion: sandbox forbids local listeners");
                return;
            }
            Err(error) => panic!("first bind: {error}"),
        };
        let port = first.local_addr().expect("first address").port();
        let mut policy = LanServerPolicy::default();
        policy.port = port;
        let error = bind_policy(&policy)
            .await
            .expect_err("second bind must fail");
        assert!(error.contains("Could not bind"));
    }

    #[test]
    fn invalid_tls_material_and_unsafe_key_references_fail_closed() {
        let invalid = StoredTlsIdentity {
            certificate_pem: "not a certificate".to_string(),
            private_key_pem: "not a key".to_string(),
        };
        assert!(tls_config_from_identity(&invalid, "1.3").is_err());
        assert!(validate_reference("../private-key").is_err());
        assert!(validate_reference("safe:key-reference_1").is_ok());
    }

    #[tokio::test]
    async fn failed_restart_preflight_preserves_the_running_listener_snapshot() {
        let state = M3HttpServerState::default();
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_shutdown.cancelled().await;
        });
        {
            let mut inner = lock_inner(&state).expect("server state");
            inner.status = "running".to_string();
            inner.bind_address = Some("127.0.0.1".to_string());
            inner.port = Some(1234);
            inner.tls = false;
            inner.started_at_ms = Some(42);
            inner.shutdown = Some(shutdown.clone());
            inner.task = Some(task);
        }

        let mut replacement = LanServerPolicy::default();
        replacement.port = 4321;
        record_start_preflight_error(&state, &replacement, "replacement TLS identity is invalid")
            .expect("record preflight error");

        let status = {
            let inner = lock_inner(&state).expect("server state");
            snapshot(&inner)
        };
        assert_eq!(status.status, "running");
        assert_eq!(status.bind_address.as_deref(), Some("127.0.0.1"));
        assert_eq!(status.port, Some(1234));
        assert!(!status.tls);
        assert_eq!(status.started_at_ms, Some(42));
        assert_eq!(
            status.last_error.as_deref(),
            Some("replacement TLS identity is invalid")
        );
        assert!(!shutdown.is_cancelled());

        stop_server_core(&state)
            .await
            .expect("cleanup listener task");
    }

    #[tokio::test]
    async fn managed_loopback_listener_serves_health_rejects_tools_and_requires_auth() {
        let root = TestRoot::new();
        let hub = test_hub(&root);
        let Some(port) = free_loopback_port().await else {
            return;
        };
        let mut policy = LanServerPolicy::default();
        policy.port = port;
        hub.configure_lan(policy)
            .expect("configure loopback policy");
        let state = M3HttpServerState::default();
        let started = start_server_core(&state, hub)
            .await
            .expect("start M3 HTTP server");
        assert_eq!(started.status, "running");
        assert_eq!(started.port, Some(port));
        assert!(!started.tls);

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("loopback client");
        let health = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .expect("health request");
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        let payload: Value = health.json().await.expect("health JSON");
        assert_eq!(payload["service"], "little-monkey-m3");
        assert_eq!(payload["conformance"]["workspaceToolRoutesExposed"], false);

        let tool = client
            .post(format!("http://127.0.0.1:{port}/v1/tool_run_shell"))
            .body("{}")
            .send()
            .await
            .expect("tool probe");
        assert_eq!(tool.status(), reqwest::StatusCode::NOT_FOUND);

        let models = client
            .get(format!("http://127.0.0.1:{port}/v1/models"))
            .send()
            .await
            .expect("models request");
        assert_eq!(models.status(), reqwest::StatusCode::UNAUTHORIZED);

        let stopped = stop_server_core(&state).await.expect("stop M3 HTTP server");
        assert_eq!(stopped.status, "stopped");
    }
}
