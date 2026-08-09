//! Managed HTTP/SSE listener for the M3 runtime and compatibility hub.
//!
//! The managed listener is a compatibility adapter around the same typed,
//! transport-free [`M3HttpRequestService`] used by the unified HTTP router. It
//! binds the exact persisted [`LanServerPolicy`], performs M3 scoped-token
//! authorization for every protected request, and exposes only compatibility
//! inference, model lifecycle, cancellation, and health. It never routes
//! workspace, file, shell, Git, MCP, recipe, or agent tools.
//!
//! # K8 scheduler backpressure: nothing to honour here
//!
//! Because of that last sentence, this listener produces no daemon work and the
//! K8 backpressure signal has no refusal to express on any of its routes — there
//! is not one mention of the daemon anywhere in this file. `server_busy` is the
//! only "too much" this listener can say, and it is a *different* condition:
//! [`RequestAdmission`] bounds requests concurrently in flight and releases as
//! soon as a response ends, so it answers `503`/`server_busy`. Scheduler
//! backpressure would mean the run queue behind the listener is full, which
//! drains on a run's timescale rather than a request's, and would be `429` with
//! `Retry-After`. A caller therefore tells them apart by status and code, and
//! must not treat one as a synonym for the other.
//!
//! [`RequestAdmission`]: crate::http_policy::RequestAdmission

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures_util::{stream, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
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
use tauri::AppHandle;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::compatibility_hub::{
    protocol_error_response, rfc3339_from_seconds, translate_embeddings_request,
    translate_ollama_chat_request, translate_request, ApiBackend, ApiScope, CompatibilityError,
    CompatibilityProtocol, LanServerPolicy, ProtocolStreamFrame, TlsPolicy,
};
use crate::http_model_catalog::{
    CatalogAuthorization, CatalogDispatchTarget, CatalogError, CatalogModel, CatalogPolicy,
    CatalogRequestContext, ModelCatalogSource,
};
use crate::http_model_service::{
    openai_model_list, HttpModelService, ModelListRequest, ModelResolveRequest,
};
use crate::http_model_sources::{catalog_backends, ProviderCredentialSource};
use crate::http_policy::{
    full_body, hold_admission_until_response_ends, json_response, unix_time_ms as now_ms, BoxError,
    CappedBodyRejection, RequestAdmission, ResponseBody, MAX_ACTIVE_REQUESTS,
};
use crate::http_route_registry::{
    classify_request, AuthFamily, ClassificationInput, HttpMethod, ListenerExposure, RouteDecision,
    RouteId, RouteOwner,
};
use crate::m3_runtime_hub::{
    M3ApiCaller, M3ApiDispatchRequest, M3CancelInferenceRequest, M3DeleteModelRequest,
    M3DownloadRequest, M3EmbeddingDispatchRequest, M3ExternalBackendCandidateAuthorization,
    M3ExternalStagedAuthorization, M3HubError, M3HubResult, M3InstalledModelView,
    M3LoadModelRequest, M3OllamaChatDispatchRequest, M3OperationContext, M3ProtocolFrameSink,
    M3RequestPrincipal, M3RuntimeCapabilityView, M3RuntimeHub, M3RuntimeKind, M3RuntimeStatusView,
    M3UnloadModelRequest,
};

use crate::http_policy::MAX_REQUEST_BODY_BYTES;

const TLS_KEYCHAIN_SERVICE: &str = "com.littlemonkey.m3.lan-tls";
const MAX_TLS_PEM_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const RUNTIME_HEADER: HeaderName = HeaderName::from_static("x-little-monkey-runtime-id");
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

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

pub(crate) async fn tls_acceptor(policy: &LanServerPolicy) -> Result<Option<TlsAcceptor>, String> {
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

#[tauri::command]
pub async fn m3_http_server_start(app: AppHandle) -> Result<M3HttpServerStatus, String> {
    crate::server::m3_http_server_start_core(&app).await
}

#[tauri::command]
pub async fn m3_http_server_stop(app: AppHandle) -> Result<M3HttpServerStatus, String> {
    crate::server::m3_http_server_stop_core(&app).await
}

#[tauri::command]
pub fn m3_http_server_status(app: AppHandle) -> Result<M3HttpServerStatus, String> {
    crate::server::m3_http_server_status_core(&app)
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

/// App/runtime-independent M3 HTTP application service.
///
/// Socket ownership, Hyper's [`Incoming`] body, and shared admission permits
/// deliberately stay outside this type. The unified listener can therefore
/// classify a route once, buffer it once, and invoke exactly the same M3
/// implementation as this module's managed HTTP listener.
#[derive(Clone)]
pub(crate) struct M3HttpRequestService {
    hub: Arc<M3RuntimeHub>,
    model_service: HttpModelService,
    model_extensions: M3HttpModelExtensions,
}

#[derive(Clone, Default)]
pub(crate) struct M3HttpModelExtensions {
    pub(crate) sources: Vec<Arc<dyn ModelCatalogSource>>,
    pub(crate) provider_base_urls: BTreeMap<String, String>,
    pub(crate) cloud_client: Option<reqwest::Client>,
    pub(crate) provider_credentials: Option<Arc<dyn ProviderCredentialSource>>,
}

impl M3HttpRequestService {
    pub(crate) fn new(hub: Arc<M3RuntimeHub>) -> Self {
        let model_service = HttpModelService::for_m3_hub(hub.clone());
        Self {
            hub,
            model_service,
            model_extensions: M3HttpModelExtensions::default(),
        }
    }

    pub(crate) fn with_model_service(
        hub: Arc<M3RuntimeHub>,
        model_service: HttpModelService,
    ) -> Self {
        Self {
            hub,
            model_service,
            model_extensions: M3HttpModelExtensions::default(),
        }
    }

    pub(crate) fn with_model_extensions(mut self, extensions: M3HttpModelExtensions) -> Self {
        self.model_extensions = extensions;
        self
    }

    pub(crate) async fn dispatch_resolved_internal_runtime(
        &self,
        route: RouteId,
        target: CatalogDispatchTarget,
        body: Bytes,
        context: M3OperationContext,
    ) -> Response<ResponseBody> {
        let CatalogDispatchTarget::Runtime {
            model_id,
            runtime_id,
            ..
        } = target
        else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_dispatch_target",
                "Internal runtime dispatch requires a resolved runtime target",
            );
        };
        let request_id = format!("internal-{}", Uuid::new_v4().simple());
        match route {
            RouteId::ChatCompletions => {
                let canonical = match translate_request(
                    CompatibilityProtocol::OpenAiChatCompletions,
                    &request_id,
                    &body,
                ) {
                    Ok(canonical) => canonical,
                    Err(error) => {
                        return translation_error_response(
                            CompatibilityProtocol::OpenAiChatCompletions,
                            &error,
                        )
                    }
                };
                if canonical.model != model_id {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_dispatch_target",
                        "Resolved model does not match the request envelope",
                    );
                }
                dispatch_runtime_inference(
                    self.hub.clone(),
                    CompatibilityProtocol::OpenAiChatCompletions,
                    runtime_id,
                    request_id,
                    body,
                    canonical.stream,
                    M3RequestPrincipal::Internal,
                    &context,
                )
                .await
            }
            RouteId::Embeddings => {
                let canonical = match translate_embeddings_request(&request_id, &body) {
                    Ok(canonical) => canonical,
                    Err(error) => {
                        return translation_error_response(
                            CompatibilityProtocol::OpenAiChatCompletions,
                            &error,
                        )
                    }
                };
                if canonical.model != model_id {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_dispatch_target",
                        "Resolved model does not match the request envelope",
                    );
                }
                dispatch_runtime_embeddings(&self.hub, runtime_id, request_id, body, &context).await
            }
            _ => error_response(
                StatusCode::BAD_REQUEST,
                "invalid_dispatch_route",
                "Resolved internal runtime dispatch supports only chat and embeddings",
            ),
        }
    }
}

/// Fully transport-free input to [`M3HttpRequestService`].
///
/// `route` is the typed result of the shared registry. The body has already
/// passed the listener's byte cap and `context.cancellation` is tied to the
/// outer shared admission guard, including for streaming responses.
pub(crate) struct M3HttpServiceRequest {
    pub(crate) route: RouteId,
    pub(crate) method: Method,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
    pub(crate) remote_address: IpAddr,
    pub(crate) policy: Arc<LanServerPolicy>,
    pub(crate) context: M3OperationContext,
}

#[derive(Clone)]
enum HttpAuth {
    Internal {
        allowed_backends: std::collections::BTreeSet<ApiBackend>,
    },
    External {
        bearer_token: String,
        remote_address: String,
    },
}

fn empty_response(status: StatusCode) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(full_body(Bytes::new()))
        .expect("fixed empty M3 response is valid")
}

/// This router's error envelope: `{"error":{"code","message","type"}}` with a
/// `little_monkey_m3_error` discriminator.
///
/// Deliberately **not** shared with `server.rs`'s same-named helper, which emits
/// the OpenAI `{"error":{"message","type":"invalid_request_error","code"}}` shape
/// and takes its arguments in the other order. Both envelopes are a
/// client-visible contract, so merging them would be a wire regression rather
/// than a cleanup — see the note at
/// `http_route_registry::shared_route_owner`.
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
        // 413, not the 502 this used to fall into as a `Runtime`: nothing on
        // this side failed. The prompt is larger than the budget set for the
        // process serving it, the request was never forwarded, and shortening it
        // is the client's move — which is exactly what 413 asks for. The code
        // carries the class's policy so a client knows whether shortening is
        // even the right response, or whether this work is meant to stop.
        M3HubError::ContextBudget { code, .. } => (StatusCode::PAYLOAD_TOO_LARGE, *code),
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

fn origin_allowed(policy: &LanServerPolicy, origin: &str) -> bool {
    policy
        .cors_allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(origin))
}

fn request_origin(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .map(str::to_string)
        .unwrap_or_else(|| format!("http-{}", Uuid::new_v4().simple()))
}

fn secure_response_for_headers(
    response: &mut Response<ResponseBody>,
    policy: &LanServerPolicy,
    headers: &HeaderMap,
) {
    let origin = request_origin(headers);
    let request_id = request_id(headers);
    apply_security_headers(response, policy, origin.as_deref(), Some(&request_id));
}

/// M3-shaped overload response for a policy-only endpoint. The unified
/// listener owns admission, but the compatibility surface still owns its
/// externally visible error envelope and security headers.
pub(crate) fn server_busy_response(
    policy: &LanServerPolicy,
    headers: &HeaderMap,
) -> Response<ResponseBody> {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "server_busy",
        "The M3 server active-request quota is exhausted",
    );
    secure_response_for_headers(&mut response, policy, headers);
    response
}

pub(crate) fn route_not_found_response(
    policy: &LanServerPolicy,
    headers: &HeaderMap,
) -> Response<ResponseBody> {
    let mut response = error_response(StatusCode::NOT_FOUND, "not_found", "Route does not exist");
    secure_response_for_headers(&mut response, policy, headers);
    response
}

/// This adapter is already the M3-owned endpoint. `PairedLanToken` here is a
/// route-owner selector, not an authentication result; token validity remains
/// exclusively enforced by [`request_auth`] inside the transport-free service.
fn classify_m3_listener_request<'path>(
    method: &Method,
    path: &'path str,
    policy: &LanServerPolicy,
) -> RouteDecision<'path> {
    let exposure = if policy.is_loopback() {
        ListenerExposure::Loopback
    } else {
        ListenerExposure::Lan
    };
    classify_request(
        method,
        path,
        ClassificationInput::new(exposure, AuthFamily::PairedLanToken),
    )
}

fn method_not_allowed_response(allowed: &[HttpMethod]) -> Response<ResponseBody> {
    let mut response = error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "This M3 route does not support the requested method",
    );
    let allow = allowed
        .iter()
        .map(|method| match method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Other => "",
        })
        .filter(|method| !method.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(value) = HeaderValue::from_str(&allow) {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
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
        None if !policy.require_authentication && policy.is_loopback() => Ok(HttpAuth::Internal {
            allowed_backends: policy.allowed_backends.clone(),
        }),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing_bearer_token",
            "A paired M3 bearer token is required",
        )),
    }
}

fn preflight_request_auth(
    hub: &M3RuntimeHub,
    headers: &HeaderMap,
    policy: &LanServerPolicy,
    remote_address: IpAddr,
) -> Result<HttpAuth, Response<ResponseBody>> {
    let auth = request_auth(headers, policy, remote_address)?;
    if let HttpAuth::External {
        bearer_token,
        remote_address,
    } = &auth
    {
        hub.preflight_external_credential(bearer_token, remote_address, now_ms())
            .map_err(hub_error_response)?;
    }
    Ok(auth)
}

#[derive(Clone, Debug)]
struct ResolutionAuthorization {
    backends: std::collections::BTreeSet<ApiBackend>,
    /// Empty means unrestricted. Non-empty is applied both to exact model
    /// resolution and to every discovery row before it reaches the client.
    allowed_models: std::collections::BTreeSet<String>,
    /// `None` means an internal caller. External staged receipts carry the
    /// complete token scope set so a protocol discovered after parsing can
    /// be narrowed without another quota-bearing authorization.
    allowed_scopes: Option<std::collections::BTreeSet<ApiScope>>,
    /// Trusted dispatch owner. This is derived from the authorization receipt
    /// and never deserialized from an HTTP or IPC request body.
    principal: M3RequestPrincipal,
}

fn catalog_authorization(authorization: &ResolutionAuthorization) -> CatalogAuthorization {
    CatalogAuthorization::Authorized {
        allowed_backends: catalog_backends(&authorization.backends),
    }
}

fn catalog_policy(policy: &LanServerPolicy) -> CatalogPolicy {
    CatalogPolicy {
        enabled_backends: catalog_backends(&policy.allowed_backends),
    }
}

fn catalog_context(context: &M3OperationContext) -> CatalogRequestContext {
    CatalogRequestContext::with_timeout(
        context.cancellation.clone(),
        std::time::Duration::from_millis(context.timeout_ms.max(1)),
    )
}

fn catalog_error_response(error: CatalogError) -> Response<ResponseBody> {
    let (status, code) = match &error {
        CatalogError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
        CatalogError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        CatalogError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        CatalogError::Cancelled => (StatusCode::REQUEST_TIMEOUT, "cancelled"),
        CatalogError::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
        CatalogError::InvalidRequest(_) | CatalogError::InvalidSource(_) => {
            (StatusCode::BAD_REQUEST, "invalid_request")
        }
        CatalogError::LimitExceeded { .. } => {
            (StatusCode::PAYLOAD_TOO_LARGE, "catalog_limit_exceeded")
        }
        CatalogError::SourceUnavailable { .. } => {
            (StatusCode::BAD_GATEWAY, "model_source_unavailable")
        }
        CatalogError::NotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
        CatalogError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
    };
    let retry_after_seconds = match &error {
        CatalogError::RateLimited { retry_after_ms } => Some((*retry_after_ms).div_ceil(1_000)),
        _ => None,
    };
    let mut response = error_response(status, code, error.to_string());
    if let Some(seconds) = retry_after_seconds {
        if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

fn catalog_error_to_hub(error: CatalogError) -> M3HubError {
    match error {
        CatalogError::Unauthorized => {
            M3HubError::Unauthorized("model catalog authentication failed".to_string())
        }
        CatalogError::Forbidden => {
            M3HubError::Forbidden("model catalog access is forbidden".to_string())
        }
        CatalogError::RateLimited { retry_after_ms } => M3HubError::RateLimited { retry_after_ms },
        CatalogError::Cancelled => M3HubError::Cancelled {
            operation: "model catalog".to_string(),
        },
        CatalogError::DeadlineExceeded => M3HubError::Timeout {
            operation: "model catalog".to_string(),
            timeout_ms: REQUEST_TIMEOUT_MS,
        },
        CatalogError::InvalidRequest(message) | CatalogError::InvalidSource(message) => {
            M3HubError::Invalid {
                field: "model".to_string(),
                message,
            }
        }
        CatalogError::LimitExceeded { .. } | CatalogError::SourceUnavailable { .. } => {
            M3HubError::Transport(error.to_string())
        }
        CatalogError::NotFound { .. } => M3HubError::NotFound(error.to_string()),
        CatalogError::Conflict(message) => M3HubError::Conflict(message),
    }
}

fn runtime_override(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(&RUNTIME_HEADER)
        .and_then(|value| value.to_str().ok())
}

async fn resolve_catalog_model(
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    headers: &HeaderMap,
    authorization: &ResolutionAuthorization,
    policy: &LanServerPolicy,
    model_id: &str,
    context: &M3OperationContext,
) -> Result<CatalogModel, CatalogError> {
    let request_context = catalog_context(context);
    let policy = catalog_policy(policy);
    model_service
        .resolve(ModelResolveRequest {
            authorization: catalog_authorization(authorization),
            policy: &policy,
            allowed_models: &authorization.allowed_models,
            model_id,
            runtime_override: runtime_override(headers),
            extra_sources: &extensions.sources,
            context: &request_context,
        })
        .await
}

fn authorize_resolution(
    hub: &M3RuntimeHub,
    auth: &HttpAuth,
    scope: ApiScope,
    model_id: Option<String>,
    input_bytes: u64,
    destructive_confirmation: Option<String>,
    deferred_destructive_resource_id: Option<String>,
) -> M3HubResult<ResolutionAuthorization> {
    let HttpAuth::External {
        bearer_token,
        remote_address,
    } = auth
    else {
        let HttpAuth::Internal { allowed_backends } = auth else {
            unreachable!("HttpAuth has only internal and external variants")
        };
        return Ok(ResolutionAuthorization {
            backends: allowed_backends.clone(),
            allowed_models: std::collections::BTreeSet::new(),
            allowed_scopes: None,
            principal: M3RequestPrincipal::Internal,
        });
    };
    let receipt =
        hub.authorize_external_backend_candidates(&M3ExternalBackendCandidateAuthorization {
            bearer_token: bearer_token.clone(),
            scope,
            model_id,
            input_bytes,
            remote_address: remote_address.clone(),
            destructive_confirmation,
            deferred_destructive_resource_id,
            now_ms: now_ms(),
        })?;
    Ok(ResolutionAuthorization {
        principal: M3RequestPrincipal::PairedToken(receipt.token_id),
        backends: receipt.backends,
        allowed_models: receipt.allowed_models,
        allowed_scopes: None,
    })
}

fn authorize_staged_resolution(
    hub: &M3RuntimeHub,
    auth: &HttpAuth,
    scope: Option<ApiScope>,
    input_bytes: u64,
) -> M3HubResult<ResolutionAuthorization> {
    let HttpAuth::External {
        bearer_token,
        remote_address,
    } = auth
    else {
        let HttpAuth::Internal { allowed_backends } = auth else {
            unreachable!("HttpAuth has only internal and external variants")
        };
        return Ok(ResolutionAuthorization {
            backends: allowed_backends.clone(),
            allowed_models: std::collections::BTreeSet::new(),
            allowed_scopes: None,
            principal: M3RequestPrincipal::Internal,
        });
    };
    let receipt = hub.authorize_external_staged_request(&M3ExternalStagedAuthorization {
        bearer_token: bearer_token.clone(),
        scope,
        input_bytes,
        remote_address: remote_address.clone(),
        now_ms: now_ms(),
    })?;
    Ok(ResolutionAuthorization {
        principal: M3RequestPrincipal::PairedToken(receipt.token_id),
        backends: receipt.backends,
        allowed_models: receipt.allowed_models,
        allowed_scopes: Some(receipt.allowed_scopes),
    })
}

fn ensure_scope_allowed(
    authorization: &ResolutionAuthorization,
    scope: ApiScope,
) -> M3HubResult<()> {
    if authorization
        .allowed_scopes
        .as_ref()
        .is_none_or(|scopes| scopes.contains(&scope))
    {
        Ok(())
    } else {
        Err(M3HubError::Forbidden(
            "token does not grant the requested scope".to_string(),
        ))
    }
}

fn ensure_model_allowed(
    authorization: &ResolutionAuthorization,
    model_id: &str,
) -> M3HubResult<()> {
    if authorization.allowed_models.is_empty() || authorization.allowed_models.contains(model_id) {
        Ok(())
    } else {
        Err(M3HubError::Forbidden(
            "token is not scoped to the requested model".to_string(),
        ))
    }
}

fn ensure_backend_allowed(
    authorization: &ResolutionAuthorization,
    backend: ApiBackend,
) -> M3HubResult<()> {
    authorization
        .backends
        .contains(&backend)
        .then_some(())
        .ok_or_else(|| {
            M3HubError::Forbidden(
                "token or server policy forbids the requested backend".to_string(),
            )
        })
}

fn filter_runtime_status(
    status: M3RuntimeStatusView,
    visible_models: &std::collections::BTreeSet<String>,
) -> M3RuntimeStatusView {
    if visible_models.is_empty() {
        return status;
    }
    match status {
        M3RuntimeStatusView::Adapter {
            status,
            mut running_models,
        } => {
            running_models.retain(|model| visible_models.contains(&model.model_id));
            M3RuntimeStatusView::Adapter {
                status,
                running_models,
            }
        }
        #[cfg(target_os = "macos")]
        M3RuntimeStatusView::Mlx {
            status:
                crate::mlx_runtime::MlxRuntimeStatus::Running {
                    capabilities,
                    package_version,
                    handle,
                    metrics: _,
                },
        } if !visible_models.contains(&handle.model_id) => M3RuntimeStatusView::Mlx {
            // This is the caller-relative view: the runtime has no visible
            // running model. Never serialize another model's handle/metrics.
            status: crate::mlx_runtime::MlxRuntimeStatus::Stopped {
                capabilities,
                package_version,
            },
        },
        // Only reachable while the MLX arm above exists — without it, `Adapter`
        // is the whole enum and a catch-all would be dead.
        #[cfg(target_os = "macos")]
        other => other,
    }
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

fn runtime_by_hub_id_authorized(
    hub: &M3RuntimeHub,
    runtime_id: &str,
    authorization: &ResolutionAuthorization,
) -> M3HubResult<M3RuntimeCapabilityView> {
    let runtime = runtime_by_hub_id(hub, runtime_id)?;
    ensure_backend_allowed(authorization, runtime.descriptor.api_backend).map_err(|_| {
        // A valid token without this backend must not learn whether a runtime
        // id exists, so collapse both cases to the same result.
        M3HubError::NotFound("authorized runtime".to_string())
    })?;
    Ok(runtime)
}

fn installed_by_asset(hub: &M3RuntimeHub, asset_id: &str) -> M3HubResult<M3InstalledModelView> {
    hub.list_installed_models()?
        .into_iter()
        .find(|model| model.asset_id == asset_id)
        .ok_or_else(|| M3HubError::NotFound(format!("installed model asset {asset_id}")))
}

/// The shared capped body read ([`crate::http_policy::read_capped_body`]) in
/// this router's own wire bytes.
///
/// The read *semantics* — never buffering past the cap, ignoring
/// `Content-Length`, dropping rather than draining a rejected body — live in
/// `http_policy.rs` so a change to them cannot take effect on one listener and
/// silently not on the other. Only the rendering is local, and it has to be:
/// this router owes the `little_monkey_m3_error` envelope (and, for a
/// cancellation, its hub-error mapping), where the legacy router owes the
/// OpenAI envelope plus a wildcard CORS header.
async fn read_capped_body<B>(
    body: B,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Bytes, Response<ResponseBody>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    crate::http_policy::read_capped_body(body, limit, cancellation)
        .await
        .map_err(|rejection| match rejection {
            CappedBodyRejection::Cancelled => hub_error_response(M3HubError::Cancelled {
                operation: "read request body".to_string(),
            }),
            CappedBodyRejection::TooLarge { limit } => error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                format!("Request body exceeds the {limit}-byte limit"),
            ),
            CappedBodyRejection::ReadFailed => error_response(
                StatusCode::BAD_REQUEST,
                "body_read_error",
                "The request body could not be read completely",
            ),
        })
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

fn sse_body(receiver: mpsc::Receiver<Bytes>) -> ResponseBody {
    let stream = stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<Frame<Bytes>, Infallible>(Frame::data(bytes)), receiver))
    });
    StreamBody::new(stream)
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed()
}

fn sse_response(receiver: mpsc::Receiver<Bytes>) -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONNECTION, "keep-alive")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(sse_body(receiver))
        .expect("fixed M3 SSE response is valid")
}

async fn provider_inference_response(
    extensions: &M3HttpModelExtensions,
    protocol: CompatibilityProtocol,
    provider_id: &str,
    provider_model_id: &str,
    parsed: &Value,
    stream_response: bool,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let Some(base_url) = extensions.provider_base_urls.get(provider_id) else {
        return hub_error_response(M3HubError::NotFound(format!(
            "configured provider {provider_id}"
        )));
    };
    let Some(client) = extensions.cloud_client.as_ref() else {
        return hub_error_response(M3HubError::State(
            "cloud-provider HTTP client is unavailable".to_string(),
        ));
    };
    let Some(credentials) = extensions.provider_credentials.as_ref() else {
        return hub_error_response(M3HubError::State(
            "cloud-provider credential resolver is unavailable".to_string(),
        ));
    };
    let api_key = match credentials.bearer_token(provider_id) {
        Ok(Some(key)) => key,
        Ok(None) | Err(_) => {
            return hub_error_response(M3HubError::Transport(
                "provider is not configured".to_string(),
            ))
        }
    };
    let endpoint = match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => "chat/completions",
        CompatibilityProtocol::OpenAiResponses => "responses",
        CompatibilityProtocol::AnthropicMessages => "messages",
    };
    let mut outgoing = parsed.clone();
    outgoing["model"] = json!(provider_model_id);
    let request = client
        .post(format!("{}/{endpoint}", base_url.trim_end_matches('/')))
        .bearer_auth(&api_key)
        .json(&outgoing);
    let request = crate::providers::add_anthropic_headers(request, provider_id, &api_key);
    let sent = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            return hub_error_response(M3HubError::Cancelled {
                operation: "provider inference".to_string(),
            });
        }
        // Metered. The wrapper counts frames as they are polled and forwards
        // `size_hint`/`is_end_stream` unchanged, so the streaming branch below
        // still hands the client upstream's exact bytes; the timeout continues to
        // bound only the send, not the body read.
        result = tokio::time::timeout(
            std::time::Duration::from_millis(context.timeout_ms.max(1)),
            crate::egress::send(request),
        ) => result,
    };
    let upstream = match sent {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return hub_error_response(M3HubError::Transport(format!(
                "provider request failed: {error}"
            )))
        }
        Err(_) => {
            return hub_error_response(M3HubError::Timeout {
                operation: "provider inference".to_string(),
                timeout_ms: context.timeout_ms,
            })
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| {
            HeaderValue::from_static(if stream_response {
                "text/event-stream"
            } else {
                "application/json"
            })
        });
    if stream_response {
        let cancellation = context.cancellation.clone();
        let stream = stream::unfold(
            (upstream.bytes_stream(), cancellation, false),
            |(mut upstream, cancellation, finished)| async move {
                if finished {
                    return None;
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        let error = std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "provider stream cancelled",
                        );
                        Some((
                            Err(Box::new(error) as BoxError),
                            (upstream, cancellation, true),
                        ))
                    }
                    next = upstream.next() => next.map(|chunk| {
                        (
                            chunk.map(Frame::data).map_err(|error| Box::new(error) as BoxError),
                            (upstream, cancellation, false),
                        )
                    }),
                }
            },
        );
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(BodyExt::boxed(StreamBody::new(stream)))
            .expect("provider streaming response metadata is valid");
    }
    let bytes = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            return hub_error_response(M3HubError::Cancelled {
                operation: "provider inference body".to_string(),
            });
        }
        result = tokio::time::timeout(
            std::time::Duration::from_millis(context.timeout_ms.max(1)),
            upstream.bytes(),
        ) => result,
    };
    match bytes {
        Ok(Ok(bytes)) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(full_body(bytes))
            .expect("provider buffered response metadata is valid"),
        Ok(Err(error)) => hub_error_response(M3HubError::Transport(format!(
            "provider response failed: {error}"
        ))),
        Err(_) => hub_error_response(M3HubError::Timeout {
            operation: "provider inference body".to_string(),
            timeout_ms: context.timeout_ms,
        }),
    }
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
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    policy: &LanServerPolicy,
    headers: &HeaderMap,
    auth: &HttpAuth,
    input_bytes: u64,
    context: &M3OperationContext,
) -> M3HubResult<(Value, Option<String>)> {
    // The gate owns validity, scope, model filter, and the one quota debit.
    // Nothing below this line may touch hub/runtime inventory before it.
    let authorization = authorize_resolution(
        hub,
        auth,
        ApiScope::ModelDiscover,
        None,
        input_bytes,
        None,
        None,
    )?;
    let request_context = catalog_context(context);
    let catalog_policy = catalog_policy(policy);
    let list_request = ModelListRequest {
        authorization: catalog_authorization(&authorization),
        policy: &catalog_policy,
        allowed_models: &authorization.allowed_models,
        extra_sources: &extensions.sources,
        context: &request_context,
    };
    let selected_runtime = runtime_override(headers);
    let models = match selected_runtime {
        Some(runtime_id) => {
            model_service
                .list_for_runtime(list_request, runtime_id)
                .await
        }
        None => model_service.list(list_request).await,
    }
    .map_err(catalog_error_to_hub)?;
    Ok((
        openai_model_list(&models, true),
        selected_runtime.map(str::to_string),
    ))
}

async fn dispatch_runtime_inference(
    hub: Arc<M3RuntimeHub>,
    protocol: CompatibilityProtocol,
    runtime_id: String,
    request_id: String,
    body: Bytes,
    stream: bool,
    principal: M3RequestPrincipal,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let request = M3ApiDispatchRequest {
        protocol,
        runtime_id,
        request_id,
        body: body.to_vec(),
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    if !stream {
        return match hub
            .dispatch_pre_authorized_api(&request, principal, context)
            .await
        {
            Ok(result) => {
                let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK);
                json_response(status, result.body)
            }
            Err(error) => hub_error_response(error),
        };
    }

    let (sender, receiver) = mpsc::channel(64);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let mut sink = ChannelFrameSink {
        sender: sender.clone(),
    };
    let context = context.clone();
    tokio::spawn(async move {
        if let Err(error) = hub
            .dispatch_pre_authorized_api_stream(
                &request,
                &mut sink,
                principal,
                ready_sender,
                &context,
            )
            .await
        {
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
    match ready_receiver.await {
        Ok(Ok(())) => sse_response(receiver),
        Ok(Err(error)) => hub_error_response(error),
        Err(_) => hub_error_response(M3HubError::Runtime(
            "stream dispatch ended before request registration".to_string(),
        )),
    }
}

async fn dispatch_runtime_embeddings(
    hub: &M3RuntimeHub,
    runtime_id: String,
    request_id: String,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let request = M3EmbeddingDispatchRequest {
        runtime_id,
        request_id,
        body: body.to_vec(),
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

async fn inference_response(
    hub: Arc<M3RuntimeHub>,
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    policy: &LanServerPolicy,
    protocol: CompatibilityProtocol,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    // The route fixes the scope and buffering fixes the exact byte count.
    // Charge that request before parsing an attacker-controlled envelope;
    // model/backend checks narrow this receipt below without another debit.
    let authorization = match authorize_staged_resolution(
        &hub,
        auth,
        Some(protocol_scope(protocol)),
        body.len() as u64,
    ) {
        Ok(authorization) => authorization,
        Err(error) => return hub_error_response(error),
    };
    let canonical = match translate_request(protocol, &request_id, &body) {
        Ok(request) => request,
        Err(error) => return translation_error_response(protocol, &error),
    };
    let resolved = match resolve_catalog_model(
        model_service,
        extensions,
        headers,
        &authorization,
        policy,
        &canonical.model,
        context,
    )
    .await
    {
        Ok(model) => model,
        Err(error) => return catalog_error_response(error),
    };
    let runtime_id = match resolved.into_dispatch_target() {
        Ok(CatalogDispatchTarget::Runtime { runtime_id, .. }) => runtime_id,
        Ok(CatalogDispatchTarget::Provider {
            provider_id,
            provider_model_id,
            ..
        }) => {
            return provider_inference_response(
                extensions,
                protocol,
                &provider_id,
                &provider_model_id,
                &serde_json::from_slice(&body).unwrap_or(Value::Null),
                canonical.stream,
                context,
            )
            .await;
        }
        Err(error) => return catalog_error_response(error),
    };
    let runtime = match runtime_by_hub_id_authorized(&hub, &runtime_id, &authorization) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    dispatch_runtime_inference(
        hub,
        protocol,
        runtime.descriptor.runtime_id,
        request_id,
        body,
        canonical.stream,
        authorization.principal.clone(),
        context,
    )
    .await
}

/// Handles `POST /v1/embeddings`, applying the exact same model-resolution,
/// authorization, and error-shape conventions as [`inference_response`].
/// Never fabricates a vector: [`M3RuntimeHub::dispatch_embeddings`] rejects
/// with a clear `unsupported` error when the resolved model's runtime does
/// not genuinely reach an embeddings-capable backend.
async fn embeddings_response(
    hub: Arc<M3RuntimeHub>,
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    policy: &LanServerPolicy,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let authorization = match authorize_staged_resolution(
        &hub,
        auth,
        Some(ApiScope::Embeddings),
        body.len() as u64,
    ) {
        Ok(authorization) => authorization,
        Err(error) => return hub_error_response(error),
    };
    // `/v1/embeddings` is an OpenAI-shaped route, so a translation failure
    // uses the OpenAI error envelope (there is no dedicated embeddings
    // protocol variant — the envelope is identical across OpenAI routes).
    let canonical = match translate_embeddings_request(&request_id, &body) {
        Ok(request) => request,
        Err(error) => {
            return translation_error_response(CompatibilityProtocol::OpenAiChatCompletions, &error)
        }
    };
    let resolved = match resolve_catalog_model(
        model_service,
        extensions,
        headers,
        &authorization,
        policy,
        &canonical.model,
        context,
    )
    .await
    {
        Ok(model) => model,
        Err(error) => return catalog_error_response(error),
    };
    let runtime_id = match resolved.into_dispatch_target() {
        Ok(CatalogDispatchTarget::Runtime { runtime_id, .. }) => runtime_id,
        Ok(CatalogDispatchTarget::Provider { .. }) => {
            return hub_error_response(M3HubError::Unsupported(
                "cloud-provider embeddings are not supported".to_string(),
            ));
        }
        Err(error) => return catalog_error_response(error),
    };
    let runtime = match runtime_by_hub_id_authorized(&hub, &runtime_id, &authorization) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    dispatch_runtime_embeddings(
        &hub,
        runtime.descriptor.runtime_id,
        request_id,
        body,
        context,
    )
    .await
}

/// Handles the Ollama-native `POST /api/chat`, reusing the `ChatCompletions`
/// scope (same operation as `/v1/chat/completions`, different wire format —
/// not a new, less-guarded route class) and the same model-resolution and
/// authorization conventions as [`inference_response`]. Always calls the
/// backend non-streaming; see [`M3RuntimeHub::dispatch_ollama_chat`]'s doc
/// for the documented streaming limitation.
async fn ollama_chat_response(
    hub: Arc<M3RuntimeHub>,
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    policy: &LanServerPolicy,
    headers: &HeaderMap,
    request_id: String,
    auth: &HttpAuth,
    body: Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let authorization = match authorize_staged_resolution(
        &hub,
        auth,
        Some(ApiScope::ChatCompletions),
        body.len() as u64,
    ) {
        Ok(authorization) => authorization,
        Err(error) => return hub_error_response(error),
    };
    let (canonical, _stream_requested) = match translate_ollama_chat_request(&request_id, &body) {
        Ok(value) => value,
        Err(error) => return hub_error_response(M3HubError::from(error)),
    };
    let resolved = match resolve_catalog_model(
        model_service,
        extensions,
        headers,
        &authorization,
        policy,
        &canonical.model,
        context,
    )
    .await
    {
        Ok(model) => model,
        Err(error) => return catalog_error_response(error),
    };
    let runtime_id = match resolved.into_dispatch_target() {
        Ok(CatalogDispatchTarget::Runtime { runtime_id, .. }) => runtime_id,
        Ok(CatalogDispatchTarget::Provider { .. }) => {
            return hub_error_response(M3HubError::Unsupported(
                "cloud-provider dispatch is not available on the Ollama endpoint".to_string(),
            ));
        }
        Err(error) => return catalog_error_response(error),
    };
    let runtime = match runtime_by_hub_id_authorized(&hub, &runtime_id, &authorization) {
        Ok(runtime) => runtime,
        Err(error) => return hub_error_response(error),
    };
    let request = M3OllamaChatDispatchRequest {
        runtime_id: runtime.descriptor.runtime_id,
        request_id,
        body: body.to_vec(),
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    match hub
        .dispatch_pre_authorized_ollama_chat(&request, authorization.principal.clone(), context)
        .await
    {
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
    model_service: &HttpModelService,
    extensions: &M3HttpModelExtensions,
    policy: &LanServerPolicy,
    headers: &HeaderMap,
    auth: &HttpAuth,
    input_bytes: u64,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    match discover_models(
        hub,
        model_service,
        extensions,
        policy,
        headers,
        auth,
        input_bytes,
        context,
    )
    .await
    {
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
    route: RouteId,
    auth: &HttpAuth,
    body: &Bytes,
    context: &M3OperationContext,
) -> Response<ResponseBody> {
    let scope = match route {
        RouteId::ModelDownload => ApiScope::ModelDownload,
        RouteId::ModelLoad => ApiScope::ModelLoad,
        RouteId::ModelUnload => ApiScope::ModelUnload,
        RouteId::ModelStatus => ApiScope::ModelStatus,
        RouteId::ModelDelete => ApiScope::ModelDelete,
        _ => return error_response(StatusCode::NOT_FOUND, "not_found", "Route does not exist"),
    };
    let authorization = match authorize_staged_resolution(hub, auth, Some(scope), body.len() as u64)
    {
        Ok(authorization) => authorization,
        Err(error) => return hub_error_response(error),
    };
    match route {
        RouteId::ModelDownload => {
            let request = match parse_json::<M3DownloadRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            if let Err(error) = ensure_model_allowed(&authorization, &request.model.model_id) {
                return hub_error_response(error);
            }
            if let Err(error) =
                ensure_backend_allowed(&authorization, backend_for_kind(request.model.runtime))
            {
                return hub_error_response(error);
            }
            match hub.download_model(&request, context).await {
                Ok(model) => json_response(StatusCode::CREATED, json!(model)),
                Err(error) => hub_error_response(error),
            }
        }
        RouteId::ModelLoad => {
            let request = match parse_json::<M3LoadModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let model = match installed_by_asset(hub, &request.asset_id) {
                Ok(model) => model,
                Err(error) => return hub_error_response(error),
            };
            if ensure_model_allowed(&authorization, &model.model_id).is_err() {
                return hub_error_response(M3HubError::NotFound(
                    "authorized model asset".to_string(),
                ));
            }
            let _runtime =
                match runtime_by_hub_id_authorized(hub, &request.runtime_id, &authorization) {
                    Ok(runtime) => runtime,
                    Err(error) => return hub_error_response(error),
                };
            match hub.load_model(&request, context).await {
                Ok(()) => json_response(StatusCode::OK, json!({ "loaded": true })),
                Err(error) => hub_error_response(error),
            }
        }
        RouteId::ModelUnload => {
            let request = match parse_json::<M3UnloadModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            if let Err(error) = ensure_model_allowed(&authorization, &request.model_id) {
                return hub_error_response(error);
            }
            let _runtime =
                match runtime_by_hub_id_authorized(hub, &request.runtime_id, &authorization) {
                    Ok(runtime) => runtime,
                    Err(error) => return hub_error_response(error),
                };
            match hub.unload_model(&request, context).await {
                Ok(()) => json_response(StatusCode::OK, json!({ "unloaded": true })),
                Err(error) => hub_error_response(error),
            }
        }
        RouteId::ModelStatus => {
            let request = match parse_json::<RuntimeStatusRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            if request.model_id.is_none() && !authorization.allowed_models.is_empty() {
                return hub_error_response(M3HubError::Forbidden(
                    "a model-scoped token must name modelId for runtime status".to_string(),
                ));
            }
            if let Some(model_id) = &request.model_id {
                if let Err(error) = ensure_model_allowed(&authorization, model_id) {
                    return hub_error_response(error);
                }
            }
            let _runtime =
                match runtime_by_hub_id_authorized(hub, &request.runtime_id, &authorization) {
                    Ok(runtime) => runtime,
                    Err(error) => return hub_error_response(error),
                };
            let mut visible_models = authorization.allowed_models.clone();
            if let Some(model_id) = &request.model_id {
                visible_models = std::collections::BTreeSet::from([model_id.clone()]);
            }
            let status = hub.runtime_status(&request.runtime_id, context).await;
            let inventory = hub.runtime_inventory(&request.runtime_id, context).await;
            match (status, inventory) {
                (Ok(status), Ok(mut inventory)) => {
                    if !visible_models.is_empty() {
                        inventory
                            .models
                            .retain(|model| visible_models.contains(&model.model_id));
                    }
                    let status = filter_runtime_status(status, &visible_models);
                    json_response(
                        StatusCode::OK,
                        json!({ "runtimeId": request.runtime_id, "status": status, "inventory": inventory }),
                    )
                }
                (Err(error), _) | (_, Err(error)) => hub_error_response(error),
            }
        }
        RouteId::ModelDelete => {
            let request = match parse_json::<M3DeleteModelRequest>(body) {
                Ok(request) => request,
                Err(response) => return response,
            };
            // The staged gate above already consumed this request's exact
            // byte quota. Confirmation and asset/model/backend narrowing are
            // pure receipt checks and must not authorize a second time.
            if request.confirmation != format!("DELETE {}", request.asset_id) {
                return hub_error_response(M3HubError::Forbidden(
                    "model deletion requires exact destructive confirmation".to_string(),
                ));
            }
            let model = match installed_by_asset(hub, &request.asset_id) {
                Ok(model) => model,
                Err(error) => return hub_error_response(error),
            };
            if ensure_model_allowed(&authorization, &model.model_id).is_err() {
                return hub_error_response(M3HubError::NotFound(
                    "authorized model asset".to_string(),
                ));
            }
            if ensure_backend_allowed(&authorization, backend_for_kind(model.runtime)).is_err() {
                return hub_error_response(M3HubError::NotFound(
                    "authorized model asset".to_string(),
                ));
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
    let authorization = match authorize_staged_resolution(hub, auth, None, body.len() as u64) {
        Ok(authorization) => authorization,
        Err(error) => return hub_error_response(error),
    };
    let request = match parse_json::<HttpCancelRequest>(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let not_found = || hub_error_response(M3HubError::NotFound("in-flight request".to_string()));
    let binding = match hub.in_flight_inference_binding(&request.request_id) {
        Ok(binding) => binding,
        Err(M3HubError::NotFound(_)) => return not_found(),
        Err(error) => return hub_error_response(error),
    };
    // Narrow the already-debited receipt against authoritative in-flight
    // metadata. Client-supplied protocol/model/runtime fields are assertions,
    // never the source of cancellation authority.
    if ensure_scope_allowed(&authorization, binding.scope).is_err() {
        return not_found();
    }
    if ensure_model_allowed(&authorization, &binding.model_id).is_err() {
        return not_found();
    }
    if runtime_by_hub_id_authorized(hub, &binding.runtime_id, &authorization).is_err() {
        return not_found();
    }
    if request.runtime_id != binding.runtime_id
        || request.model_id != binding.model_id
        || protocol_scope(request.protocol) != binding.scope
    {
        return not_found();
    }
    let principal = authorization.principal.clone();
    let cancel_request = M3CancelInferenceRequest {
        protocol: request.protocol,
        runtime_id: binding.runtime_id.clone(),
        request_id: request.request_id,
        model_id: binding.model_id.clone(),
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    match hub
        .cancel_pre_authorized_inference(&cancel_request, binding, principal, context)
        .await
    {
        Ok(cancelled) => json_response(StatusCode::OK, json!({ "cancelled": cancelled })),
        Err(M3HubError::Forbidden(_) | M3HubError::NotFound(_)) => not_found(),
        Err(error) => hub_error_response(error),
    }
}

impl M3HttpRequestService {
    pub(crate) async fn handle(&self, request: M3HttpServiceRequest) -> Response<ResponseBody> {
        let M3HttpServiceRequest {
            route,
            method,
            headers,
            body,
            remote_address,
            policy,
            context,
        } = request;
        let origin = request_origin(&headers);
        let request_id = request_id(&headers);

        let mut response = if origin
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
        } else if method == Method::GET && route == RouteId::Health {
            json_response(
                StatusCode::OK,
                json!({
                    "status": "ok",
                    "service": "little-monkey-m3",
                    "schemaVersion": 1,
                    "tls": matches!(policy.tls, TlsPolicy::Certificate { .. }),
                    "conformance": self.hub.conformance_manifest(),
                }),
            )
        } else if method == Method::GET && route == RouteId::Contract {
            // Answered beside `/health`, before authentication, for the reason
            // the route table gives: a client negotiates the ABI before it can
            // know its credentials still fit it.
            json_response(StatusCode::OK, crate::contract::introspection())
        } else {
            let auth = match preflight_request_auth(&self.hub, &headers, &policy, remote_address) {
                Ok(auth) => auth,
                Err(mut response) => {
                    apply_security_headers(
                        &mut response,
                        &policy,
                        origin.as_deref(),
                        Some(&request_id),
                    );
                    return response;
                }
            };
            match (method, route) {
                (Method::GET, RouteId::Models) => {
                    match discover_models(
                        &self.hub,
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        &headers,
                        &auth,
                        body.len() as u64,
                        &context,
                    )
                    .await
                    {
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
                (Method::POST, RouteId::ChatCompletions) => {
                    inference_response(
                        self.hub.clone(),
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        CompatibilityProtocol::OpenAiChatCompletions,
                        &headers,
                        request_id.clone(),
                        &auth,
                        body,
                        &context,
                    )
                    .await
                }
                (Method::POST, RouteId::OpenAiResponses) => {
                    inference_response(
                        self.hub.clone(),
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        CompatibilityProtocol::OpenAiResponses,
                        &headers,
                        request_id.clone(),
                        &auth,
                        body,
                        &context,
                    )
                    .await
                }
                (Method::POST, RouteId::AnthropicMessages) => {
                    inference_response(
                        self.hub.clone(),
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        CompatibilityProtocol::AnthropicMessages,
                        &headers,
                        request_id.clone(),
                        &auth,
                        body,
                        &context,
                    )
                    .await
                }
                (Method::POST, RouteId::Embeddings) => {
                    embeddings_response(
                        self.hub.clone(),
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        &headers,
                        request_id.clone(),
                        &auth,
                        body,
                        &context,
                    )
                    .await
                }
                (Method::GET, RouteId::OllamaTags) => {
                    ollama_tags_response(
                        &self.hub,
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        &headers,
                        &auth,
                        body.len() as u64,
                        &context,
                    )
                    .await
                }
                (Method::POST, RouteId::OllamaChat) => {
                    ollama_chat_response(
                        self.hub.clone(),
                        &self.model_service,
                        &self.model_extensions,
                        &policy,
                        &headers,
                        request_id.clone(),
                        &auth,
                        body,
                        &context,
                    )
                    .await
                }
                (
                    Method::POST,
                    route @ (RouteId::ModelDownload
                    | RouteId::ModelLoad
                    | RouteId::ModelUnload
                    | RouteId::ModelStatus
                    | RouteId::ModelDelete),
                ) => lifecycle_response(&self.hub, route, &auth, &body, &context).await,
                (Method::POST, RouteId::RequestCancel) => {
                    cancel_response(&self.hub, &auth, &body, &context).await
                }
                _ => method_not_allowed_response(&[
                    HttpMethod::Get,
                    HttpMethod::Post,
                    HttpMethod::Options,
                ]),
            }
        };
        apply_security_headers(&mut response, &policy, origin.as_deref(), Some(&request_id));
        response
    }
}

/// Transport adapter used by the unified listener after the shared registry
/// selected an M3 route.  It preserves the security ordering of the former M3
/// listener: public/denied/auth-failed requests never poll or allocate their
/// body, while admitted bodies pass one shared cap before entering the
/// transport-free service.
pub(crate) async fn handle_http_request<B>(
    service: M3HttpRequestService,
    route: RouteId,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    request: Request<B>,
    context: M3OperationContext,
) -> Response<ResponseBody>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    // Preserve the original security ordering: unsupported paths, denied
    // origins, public health/preflight, and invalid authentication never make
    // the listener poll or allocate an HTTP request body. The application
    // service repeats the pure checks so direct/unified callers cannot bypass
    // them; only the listener decides whether buffering is necessary.
    let origin_denied = request_origin(request.headers())
        .as_deref()
        .is_some_and(|origin| !origin_allowed(&policy, origin));
    let public_without_body = request.method() == Method::OPTIONS
        || (request.method() == Method::GET
            && matches!(route, RouteId::Health | RouteId::Contract));
    if !public_without_body && !origin_denied {
        if let Err(mut response) =
            preflight_request_auth(&service.hub, request.headers(), &policy, remote_address)
        {
            secure_response_for_headers(&mut response, &policy, request.headers());
            return response;
        }
    }
    let should_buffer = !public_without_body && !origin_denied;

    let (parts, incoming_body) = request.into_parts();
    let body = if should_buffer {
        match read_capped_body(incoming_body, MAX_REQUEST_BODY_BYTES, &context.cancellation).await {
            Ok(body) => body,
            Err(mut response) => {
                secure_response_for_headers(&mut response, &policy, &parts.headers);
                return response;
            }
        }
    } else {
        Bytes::new()
    };
    service
        .handle(M3HttpServiceRequest {
            route,
            method: parts.method,
            headers: parts.headers,
            body,
            remote_address,
            policy,
            context,
        })
        .await
}

/// Real-socket compatibility harness used by the crate's integration suite.
///
/// This is deliberately named and scoped as test infrastructure: desktop
/// commands, autostart, status, and exit never manage it. Production socket
/// ownership lives exclusively in `UnifiedHttpServerState`; the harness only
/// lets black-box protocol tests exercise this transport adapter without a
/// Tauri `AppHandle`.
pub struct CompatibilityHarnessServer {
    shutdown: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl CompatibilityHarnessServer {
    pub async fn stop(&self) {
        self.shutdown.cancel();
        let task = self.task.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

async fn serve_harness_connection<I>(
    io: I,
    service: M3HttpRequestService,
    policy: Arc<LanServerPolicy>,
    remote_address: IpAddr,
    admission: Arc<RequestAdmission>,
    shutdown: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection_shutdown = shutdown.clone();
    let request_policy = policy.clone();
    let http_service = service_fn(move |request: Request<hyper::body::Incoming>| {
        let service = service.clone();
        let policy = request_policy.clone();
        let admission = admission.clone();
        let shutdown = shutdown.clone();
        async move {
            let decision =
                classify_m3_listener_request(request.method(), request.uri().path(), &policy);
            let route = match decision {
                RouteDecision::Allowed(route) if route.owner == RouteOwner::M3 => route.route.id,
                RouteDecision::MethodNotAllowed {
                    owner: RouteOwner::M3,
                    route,
                    ..
                } => route,
                _ => {
                    return Ok::<_, Infallible>(route_not_found_response(
                        &policy,
                        request.headers(),
                    ));
                }
            };
            let Some(guard) = admission.try_admit(&shutdown) else {
                return Ok(server_busy_response(&policy, request.headers()));
            };
            let context = M3OperationContext {
                cancellation: guard.cancellation(),
                timeout_ms: REQUEST_TIMEOUT_MS,
            };
            let response =
                handle_http_request(service, route, policy, remote_address, request, context).await;
            Ok(hold_admission_until_response_ends(response, guard))
        }
    });
    let connection = http1::Builder::new().serve_connection(TokioIo::new(io), http_service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection_shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
        _ = &mut connection => {}
    }
}

async fn run_compatibility_harness(
    service: M3HttpRequestService,
    listener: TcpListener,
    policy: LanServerPolicy,
    acceptor: Option<TlsAcceptor>,
    shutdown: CancellationToken,
    admission: Arc<RequestAdmission>,
) {
    let policy = Arc::new(policy);
    let connection_limit = Arc::new(Semaphore::new(MAX_ACTIVE_REQUESTS * 2));
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
                let service = service.clone();
                let policy = policy.clone();
                let admission = admission.clone();
                let shutdown = shutdown.clone();
                if let Some(acceptor) = acceptor.clone() {
                    connections.spawn(async move {
                        let _connection_permit = connection_permit;
                        let accepted = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            acceptor.accept(stream),
                        ).await;
                        if let Ok(Ok(stream)) = accepted {
                            serve_harness_connection(
                                stream,
                                service,
                                policy,
                                remote.ip(),
                                admission,
                                shutdown,
                            ).await;
                        }
                    });
                } else {
                    connections.spawn(async move {
                        let _connection_permit = connection_permit;
                        serve_harness_connection(
                            stream,
                            service,
                            policy,
                            remote.ip(),
                            admission,
                            shutdown,
                        ).await;
                    });
                }
            }
        }
    }
    while connections.join_next().await.is_some() {}
}

pub async fn start_compatibility_harness(
    hub: Arc<M3RuntimeHub>,
) -> Result<(CompatibilityHarnessServer, M3HttpServerStatus), String> {
    let service = M3HttpRequestService::new(hub.clone());
    start_compatibility_harness_with_service(hub, service).await
}

async fn start_compatibility_harness_with_service(
    hub: Arc<M3RuntimeHub>,
    service: M3HttpRequestService,
) -> Result<(CompatibilityHarnessServer, M3HttpServerStatus), String> {
    let policy = hub
        .lan_policy()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Configure an M3 LAN policy before starting the harness".to_string())?;
    M3RuntimeHub::validate_lan_policy(&policy).map_err(|error| error.to_string())?;
    let acceptor = tls_acceptor(&policy).await?;
    let address = policy
        .bind_address
        .parse::<IpAddr>()
        .map_err(|error| format!("Invalid M3 bind address: {error}"))?;
    let listener = TcpListener::bind((address, policy.port))
        .await
        .map_err(|error| {
            crate::http_policy::describe_bind_error(
                crate::http_policy::ListenerRole::CompatibilityListener,
                &policy.bind_address,
                policy.port,
                &error,
            )
        })?;
    let shutdown = CancellationToken::new();
    let admission = Arc::new(RequestAdmission::new(MAX_ACTIVE_REQUESTS));
    let task = tokio::spawn(run_compatibility_harness(
        service,
        listener,
        policy.clone(),
        acceptor.clone(),
        shutdown.clone(),
        admission.clone(),
    ));
    let status = M3HttpServerStatus {
        status: "running".to_string(),
        bind_address: Some(policy.bind_address),
        port: Some(policy.port),
        tls: acceptor.is_some(),
        started_at_ms: Some(now_ms()),
        request_count: admission.request_count(),
        active_requests: admission.active_requests(),
        last_request_at_ms: None,
        last_error: None,
    };
    Ok((
        CompatibilityHarnessServer {
            shutdown,
            task: Mutex::new(Some(task)),
        },
        status,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Context, Poll};

    use crate::compatibility_hub::{LanStateProtector, OsLanEntropy, PairingRequest};
    use crate::m3_runtime_hub::{
        DefaultM3LanAccessFactory, M3DownloadTransport, M3HardwareProbe, M3HubConfig,
        M3RuntimeHubDependencies, ReqwestM3DownloadTransport, SystemM3Clock,
    };
    use crate::runtime_adapter::{
        AcceleratorCapability, AcceleratorKind, HardwareSnapshot, PlatformCapabilities,
    };

    struct TestHardware;

    struct CountingProviderCredentials {
        calls: Arc<AtomicUsize>,
        token: String,
    }

    impl ProviderCredentialSource for CountingProviderCredentials {
        fn bearer_token(
            &self,
            _provider_id: &str,
        ) -> Result<Option<String>, crate::http_model_catalog::CatalogSourceError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Some(self.token.clone()))
        }
    }

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
                    devices: Vec::new(),
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

    #[test]
    fn managed_listener_uses_the_typed_registry_as_its_only_route_authority() {
        let policy = LanServerPolicy::default();
        for (method, path) in [
            (Method::GET, "/health"),
            (Method::GET, "/v1/models"),
            (Method::POST, "/v1/chat/completions"),
            (Method::POST, "/v1/responses"),
            (Method::POST, "/v1/messages"),
            (Method::POST, "/v1/embeddings"),
            (Method::POST, "/v1/models/download"),
            (Method::POST, "/v1/models/load"),
            (Method::POST, "/v1/models/unload"),
            (Method::POST, "/v1/models/status"),
            (Method::POST, "/v1/models/delete"),
            (Method::POST, "/v1/requests/cancel"),
            (Method::GET, "/api/tags"),
            (Method::POST, "/api/chat"),
        ] {
            assert!(
                matches!(
                    classify_m3_listener_request(&method, path, &policy),
                    RouteDecision::Allowed(route) if route.owner == RouteOwner::M3
                ),
                "typed registry omitted M3 route {method} {path}"
            );
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
                !matches!(
                    classify_m3_listener_request(&Method::POST, forbidden, &policy),
                    RouteDecision::Allowed(route) if route.owner == RouteOwner::M3
                ),
                "forbidden route leaked: {forbidden}"
            );
        }

        let source = include_str!("m3_http_server.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(before, _)| before)
            .expect("m3_http_server.rs has a #[cfg(test)] module");
        assert!(!production.contains("fn is_allowed_path"));
        assert!(production.contains("fn classify_m3_listener_request<'path>("));
        assert!(!production.contains("async fn run_accept_loop("));
        let managed_authority = production
            .split_once("pub struct CompatibilityHarnessServer")
            .map(|(managed, _)| managed)
            .expect("test harness is explicitly separated from managed authority");
        assert!(!managed_authority.contains("TcpListener::bind"));
        assert!(!managed_authority.contains("M3HttpServerState"));
    }

    #[test]
    fn every_admitted_route_is_guarded_at_the_service_boundary() {
        let source = include_str!("m3_http_server.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(before, _)| before)
            .expect("m3_http_server.rs has a #[cfg(test)] module");

        assert!(production.contains("pub(crate) async fn handle_http_request<B>("));
        assert!(production.contains("&context.cancellation"));
        assert_eq!(
            production.matches(".handle(M3HttpServiceRequest {").count(),
            1,
            "only the managed listener edge may enter the transport-free HTTP service"
        );
        assert!(production.contains("pub(crate) async fn dispatch_resolved_internal_runtime("));
        let unified_transport = include_str!("server.rs");
        assert!(unified_transport.contains("serve_with_admission_response("));
        assert!(unified_transport.contains("hold_admission_until_response_ends(response, guard)"));
        assert!(
            !production.contains("struct RequestGuard"),
            "M3 must not move bespoke guard ownership into only its SSE body"
        );
    }

    struct PollCountingBody {
        polls: Arc<AtomicUsize>,
    }

    impl hyper::body::Body for PollCountingBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, AtomicOrdering::SeqCst);
            Poll::Ready(None)
        }
    }

    struct PendingCountingBody {
        polls: Arc<AtomicUsize>,
    }

    impl hyper::body::Body for PendingCountingBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, AtomicOrdering::SeqCst);
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn rejected_auth_never_polls_or_buffers_the_transport_body() {
        let root = TestRoot::new();
        let service = M3HttpRequestService::new(test_hub(&root));
        let policy = Arc::new(LanServerPolicy::default());
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/models")
            .body(PollCountingBody {
                polls: polls.clone(),
            })
            .expect("denied request");

        let response = handle_http_request(
            service,
            RouteId::Models,
            policy,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            request,
            M3OperationContext::new(REQUEST_TIMEOUT_MS),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(polls.load(AtomicOrdering::SeqCst), 0);

        let hub = test_hub(&root);
        let policy = LanServerPolicy::default();
        hub.configure_lan(policy.clone())
            .expect("configure credential preflight");
        let service = M3HttpRequestService::new(hub);
        let polls = Arc::new(AtomicUsize::new(0));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header(
                header::AUTHORIZATION,
                format!("Bearer lmk-lan-{}", "a".repeat(64)),
            )
            .body(PollCountingBody {
                polls: polls.clone(),
            })
            .expect("unknown bearer request");
        let response = handle_http_request(
            service,
            RouteId::ChatCompletions,
            Arc::new(policy),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            request,
            M3OperationContext::new(REQUEST_TIMEOUT_MS),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(polls.load(AtomicOrdering::SeqCst), 0);
    }

    /// The K19 introspection endpoint on the LAN/paired listener, answered
    /// with no `Authorization` header at all. It is deliberately in the same
    /// pre-authentication band as `/health`: a caller negotiating the ABI has
    /// not necessarily got a credential of the right shape yet, and the body
    /// is a pure function of the built binary — no configuration, no model
    /// list, no credential state — so there is nothing here to leak.
    #[tokio::test]
    async fn the_contract_route_answers_the_m3_listener_before_authentication() {
        let root = TestRoot::new();
        let service = M3HttpRequestService::new(test_hub(&root));
        let response = service
            .handle(M3HttpServiceRequest {
                route: RouteId::Contract,
                method: Method::GET,
                headers: HeaderMap::new(),
                body: Bytes::new(),
                remote_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                policy: Arc::new(LanServerPolicy::default()),
                context: M3OperationContext::new(REQUEST_TIMEOUT_MS),
            })
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("contract body")
                .to_bytes(),
        )
        .expect("contract JSON");
        assert_eq!(
            payload["contract_version"],
            crate::contract::CONTRACT_VERSION
        );
        assert_eq!(payload["digest"], crate::contract::digest());
        assert!(payload["manifest"]["http_routes"]
            .as_array()
            .is_some_and(|routes| !routes.is_empty()));
    }

    #[tokio::test]
    async fn request_service_runs_without_a_socket_incoming_body_or_app_handle() {
        let root = TestRoot::new();
        let service = M3HttpRequestService::new(test_hub(&root));
        let response = service
            .handle(M3HttpServiceRequest {
                route: RouteId::Health,
                method: Method::GET,
                headers: HeaderMap::new(),
                body: Bytes::new(),
                remote_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                policy: Arc::new(LanServerPolicy::default()),
                context: M3OperationContext::new(REQUEST_TIMEOUT_MS),
            })
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("health body")
                .to_bytes(),
        )
        .expect("health JSON");
        assert_eq!(payload["service"], "little-monkey-m3");

        let source = include_str!("m3_http_server.rs");
        let contract = source
            .split_once("pub(crate) struct M3HttpRequestService")
            .and_then(|(_, after)| after.split_once("#[derive(Clone)]\nenum HttpAuth"))
            .map(|(contract, _)| contract)
            .expect("transport-free service contract section");
        assert!(!contract.contains("Incoming"));
        assert!(!contract.contains("AppHandle"));
        assert!(!contract.contains("AdmissionGuard"));
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

    /// The cap *semantics* live in `http_policy.rs` and are tested there once,
    /// for both listeners. What is pinned here is this router's rendering of the
    /// rejection: the `little_monkey_m3_error` envelope with no CORS header,
    /// which differs from the legacy router's OpenAI envelope plus wildcard CORS
    /// on purpose. Both renderings are pinned so a change to the shared read
    /// cannot quietly reshape one side's wire bytes.
    #[tokio::test]
    async fn request_body_limit_rejects_before_buffering_past_the_quota() {
        let body = Full::new(Bytes::from_static(b"12345"));
        let response = read_capped_body(body, 4, &CancellationToken::new())
            .await
            .expect_err("oversized request must fail");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect M3 413 body")
            .to_bytes();
        assert_eq!(
            std::str::from_utf8(&bytes).expect("utf-8 body"),
            r#"{"error":{"code":"request_too_large","message":"Request body exceeds the 4-byte limit","type":"little_monkey_m3_error"}}"#
        );
    }

    /// An over-budget prompt is the client's request, not this app's failure.
    ///
    /// It used to arrive as `M3HubError::Runtime`, which renders as a 502 — a
    /// client reading that has been told the upstream broke, and its correct
    /// response to a 502 is to retry the identical request. Retrying is the one
    /// thing that cannot work here. 413 asks for the shortening that actually
    /// helps, and the code says whether shortening is even the intended move or
    /// whether this class of work is meant to stop instead.
    #[tokio::test]
    async fn an_over_budget_prompt_answers_413_with_the_classs_policy() {
        for (code, expected) in [
            ("context_budget_compact", "context_budget_compact"),
            ("context_budget_refuse", "context_budget_refuse"),
            ("context_budget", "context_budget"),
        ] {
            let response = hub_error_response(M3HubError::ContextBudget {
                code,
                message: "This request's prompt is 9000 tokens, over the 8192-token budget."
                    .to_string(),
            });
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("collect budget refusal body")
                .to_bytes();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes).expect("JSON error envelope");
            assert_eq!(body["error"]["code"], expected);
            // The message is the refusal itself, unprefixed: every other variant
            // renders as "runtime: …"/"transport: …", and this one is already a
            // complete sentence written for whoever has to act on it.
            assert_eq!(
                body["error"]["message"],
                "This request's prompt is 9000 tokens, over the 8192-token budget."
            );
        }
    }

    #[tokio::test]
    async fn stalled_m3_upload_exits_promptly_when_its_context_is_cancelled() {
        let cancellation = CancellationToken::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let body = PendingCountingBody {
            polls: polls.clone(),
        };
        let cancellation_for_task = cancellation.clone();
        let task =
            tokio::spawn(async move { read_capped_body(body, 1024, &cancellation_for_task).await });
        while polls.load(AtomicOrdering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("cancelled M3 upload must not stall")
            .expect("body task")
            .expect_err("cancellation must stop body buffering");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let polls = Arc::new(AtomicUsize::new(0));
        let response = read_capped_body(
            PollCountingBody {
                polls: polls.clone(),
            },
            1024,
            &cancellation,
        )
        .await
        .expect_err("pre-cancelled request must fail");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(polls.load(AtomicOrdering::SeqCst), 0);
    }

    fn pair_test_token(
        hub: &M3RuntimeHub,
        label: &str,
        scopes: std::collections::BTreeSet<ApiScope>,
    ) -> String {
        let challenge = hub
            .begin_pairing(
                PairingRequest {
                    client_label: label.to_string(),
                    scopes,
                    backends: std::collections::BTreeSet::from([
                        ApiBackend::ManagedLocal,
                        ApiBackend::Ollama,
                        ApiBackend::Mlx,
                    ]),
                    allowed_models: std::collections::BTreeSet::new(),
                    token_expires_at_ms: None,
                },
                1_000,
                "127.0.0.1",
            )
            .expect("begin staged-auth pairing");
        hub.complete_pairing(
            &challenge.challenge_id,
            &challenge.pairing_code,
            1_000,
            "127.0.0.1",
        )
        .expect("complete staged-auth pairing")
        .token
    }

    async fn staged_service_status(
        service: &M3HttpRequestService,
        policy: Arc<LanServerPolicy>,
        route: RouteId,
        token: &str,
        body: Bytes,
    ) -> StatusCode {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("paired bearer header"),
        );
        let response = service
            .handle(M3HttpServiceRequest {
                route,
                method: Method::POST,
                headers,
                body,
                remote_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                policy,
                context: M3OperationContext::new(REQUEST_TIMEOUT_MS),
            })
            .await;
        let status = response.status();
        status
    }

    #[tokio::test]
    async fn malformed_m3_envelopes_are_debited_exactly_once_before_parsing() {
        let root = TestRoot::new();
        let hub = test_hub(&root);
        let mut policy = LanServerPolicy::default();
        policy.rate_limit.max_requests = 100;
        policy.rate_limit.max_input_bytes = 2;
        hub.configure_lan(policy.clone())
            .expect("configure staged authorization policy");
        let service = M3HttpRequestService::new(hub.clone());
        let scopes = std::collections::BTreeSet::from([
            ApiScope::ChatCompletions,
            ApiScope::Embeddings,
            ApiScope::ModelDownload,
            ApiScope::ModelLoad,
            ApiScope::ModelUnload,
            ApiScope::ModelStatus,
            ApiScope::ModelDelete,
        ]);

        for route in [
            RouteId::ChatCompletions,
            RouteId::Embeddings,
            RouteId::OllamaChat,
            RouteId::ModelDownload,
            RouteId::ModelLoad,
            RouteId::ModelUnload,
            RouteId::ModelStatus,
            RouteId::ModelDelete,
            RouteId::RequestCancel,
        ] {
            let token = pair_test_token(&hub, &format!("malformed-{route:?}"), scopes.clone());
            let first = staged_service_status(
                &service,
                Arc::new(policy.clone()),
                route,
                &token,
                Bytes::from_static(b"{"),
            )
            .await;
            let second = staged_service_status(
                &service,
                Arc::new(policy.clone()),
                route,
                &token,
                Bytes::from_static(b"{"),
            )
            .await;
            let third = staged_service_status(
                &service,
                Arc::new(policy.clone()),
                route,
                &token,
                Bytes::from_static(b"{"),
            )
            .await;
            assert_eq!(first, StatusCode::BAD_REQUEST, "first {route:?}");
            assert_eq!(second, StatusCode::BAD_REQUEST, "second {route:?}");
            assert_eq!(third, StatusCode::TOO_MANY_REQUESTS, "third {route:?}");
        }
    }

    #[tokio::test]
    async fn valid_m3_inference_also_consumes_only_one_byte_quota_debit() {
        let body = Bytes::from_static(
            br#"{"model":"missing","messages":[{"role":"user","content":"x"}]}"#,
        );
        let root = TestRoot::new();
        let hub = test_hub(&root);
        let mut policy = LanServerPolicy::default();
        policy.rate_limit.max_requests = 100;
        policy.rate_limit.max_input_bytes = (body.len() * 2) as u64;
        hub.configure_lan(policy.clone())
            .expect("configure valid staged authorization policy");
        let token = pair_test_token(
            &hub,
            "valid-one-debit",
            std::collections::BTreeSet::from([ApiScope::ChatCompletions]),
        );
        let service = M3HttpRequestService::new(hub);
        let first = staged_service_status(
            &service,
            Arc::new(policy.clone()),
            RouteId::ChatCompletions,
            &token,
            body.clone(),
        )
        .await;
        let second = staged_service_status(
            &service,
            Arc::new(policy.clone()),
            RouteId::ChatCompletions,
            &token,
            body.clone(),
        )
        .await;
        let third = staged_service_status(
            &service,
            Arc::new(policy),
            RouteId::ChatCompletions,
            &token,
            body,
        )
        .await;
        assert_eq!(first, StatusCode::NOT_FOUND);
        assert_eq!(second, StatusCode::NOT_FOUND);
        assert_eq!(third, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn paired_provider_catalog_is_post_auth_listed_and_dispatchable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind provider fixture");
        let upstream_address = upstream.local_addr().expect("provider fixture address");
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_by_server = observed.clone();
        let upstream_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = upstream.accept().await else {
                    break;
                };
                let observed = observed_by_server.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        let Some(header_end) =
                            request.windows(4).position(|part| part == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..header_end + 4]);
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                line.split_once(':').and_then(|(name, value)| {
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                    let request_text = String::from_utf8_lossy(&request).to_string();
                    observed
                        .lock()
                        .expect("provider observations")
                        .push(request_text.clone());
                    let body: &[u8] = if request_text.starts_with("GET /v1/models ") {
                        br#"{"object":"list","data":[{"id":"gpt-fake"}]}"#
                    } else {
                        br#"{"id":"provider-dispatch-ok","object":"chat.completion"}"#
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });

        let root = TestRoot::new();
        let hub = test_hub(&root);
        let probe = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve harness port");
        let port = probe.local_addr().expect("harness address").port();
        drop(probe);
        let mut policy = LanServerPolicy::default();
        policy.port = port;
        policy.require_authentication = true;
        policy.allowed_backends = std::collections::BTreeSet::from([ApiBackend::CloudProvider]);
        policy.allow_cloud_providers_over_lan = true;
        hub.configure_lan(policy.clone())
            .expect("configure provider harness");
        let challenge = hub
            .begin_pairing(
                PairingRequest {
                    client_label: "provider test".to_string(),
                    scopes: std::collections::BTreeSet::from([
                        ApiScope::ModelDiscover,
                        ApiScope::ChatCompletions,
                    ]),
                    backends: std::collections::BTreeSet::from([ApiBackend::CloudProvider]),
                    allowed_models: std::collections::BTreeSet::new(),
                    token_expires_at_ms: None,
                },
                1_000,
                "127.0.0.1",
            )
            .expect("begin provider pairing");
        let paired = hub
            .complete_pairing(
                &challenge.challenge_id,
                &challenge.pairing_code,
                1_000,
                "127.0.0.1",
            )
            .expect("complete provider pairing");

        let credential_calls = Arc::new(AtomicUsize::new(0));
        let credentials: Arc<dyn ProviderCredentialSource> =
            Arc::new(CountingProviderCredentials {
                calls: credential_calls.clone(),
                token: "provider-secret".to_string(),
            });
        let cloud_client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("provider client");
        // Legacy provider exposure is deliberately false here. The paired
        // policy alone must construct discovery + dispatch dependencies for
        // this policy-only endpoint.
        let extensions_enabled = crate::server::provider_sources_enabled(false, Some(&policy));
        assert!(extensions_enabled);
        let extensions = crate::server::provider_model_extensions(
            extensions_enabled,
            &[crate::server::ProviderSummary {
                id: "example".to_string(),
                base_url: format!("http://{upstream_address}/v1"),
            }],
            &cloud_client,
            credentials,
        );
        let model_service = HttpModelService::for_m3_hub(hub.clone());
        let service = M3HttpRequestService::with_model_service(hub.clone(), model_service)
            .with_model_extensions(extensions);
        let (server, _) = start_compatibility_harness_with_service(hub, service)
            .await
            .expect("start provider harness");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("harness client");
        let base = format!("http://127.0.0.1:{port}");

        let invalid = client
            .get(format!("{base}/v1/models"))
            .bearer_auth("invalid-token")
            .send()
            .await
            .expect("invalid provider list request");
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(credential_calls.load(AtomicOrdering::SeqCst), 0);
        assert!(observed.lock().expect("provider observations").is_empty());

        let listing_response = client
            .get(format!("{base}/v1/models"))
            .bearer_auth(&paired.token)
            .send()
            .await
            .expect("provider list request");
        let listing_status = listing_response.status();
        let listing_text = listing_response.text().await.expect("provider list body");
        assert_eq!(
            listing_status,
            StatusCode::OK,
            "provider list: {listing_text}"
        );
        let listing: Value = serde_json::from_str(&listing_text).expect("provider list JSON");
        assert_eq!(listing["data"][0]["id"], "example/gpt-fake");

        let dispatched = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&paired.token)
            .json(&json!({
                "model": "example/gpt-fake",
                "messages": [{"role":"user","content":"hello"}],
                "stream": false
            }))
            .send()
            .await
            .expect("provider dispatch request");
        assert_eq!(dispatched.status(), StatusCode::OK);
        let dispatched_body: Value = dispatched.json().await.expect("provider dispatch JSON");
        assert_eq!(dispatched_body["id"], "provider-dispatch-ok");
        let observations = observed.lock().expect("provider observations").clone();
        let chat = observations
            .iter()
            .find(|request| request.starts_with("POST /v1/chat/completions "))
            .expect("provider chat request observed");
        assert!(chat
            .to_ascii_lowercase()
            .contains("authorization: bearer provider-secret"));
        assert!(chat.contains(r#""model":"gpt-fake""#));
        assert!(!chat.contains("example/gpt-fake"));

        let unknown = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&paired.token)
            .json(&json!({
                "model": "example/not-advertised",
                "messages": [{"role":"user","content":"hello"}]
            }))
            .send()
            .await
            .expect("unknown provider model request");
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        server.stop().await;
        upstream_task.abort();
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
}
