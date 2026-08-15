use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::desktop::DesktopControlRuntime;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::daemon::store::{restrict_file, DaemonPaths};

use super::api::{ApiRequest, ApiResponse, RemoteApi};
use super::protocol::{
    certificate_fingerprint, RemoteHostConfig, SignedRequestHeaders, MAX_REMOTE_BODY_BYTES,
    REMOTE_PROTOCOL_VERSION,
};

const HEADER_DEVICE: &str = "x-little-monkey-device";
const HEADER_GENERATION: &str = "x-little-monkey-key-generation";
const HEADER_SEQUENCE: &str = "x-little-monkey-sequence";
const HEADER_TIMESTAMP: &str = "x-little-monkey-timestamp-ms";
const HEADER_NONCE: &str = "x-little-monkey-nonce";
const HEADER_COMMAND: &str = "x-little-monkey-command";
const HEADER_SIGNATURE: &str = "x-little-monkey-signature";

pub fn host_config_path(paths: &DaemonPaths) -> PathBuf {
    paths.root.join("remote-host.json")
}

pub fn load_host_config(paths: &DaemonPaths) -> Result<Option<RemoteHostConfig>, String> {
    let path = host_config_path(paths);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let config: RemoteHostConfig = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Remote host config is invalid: {error}"))?;
            validate_host_config(&config)?;
            Ok(Some(config))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read remote host config: {error}")),
    }
}

pub fn save_host_config(paths: &DaemonPaths, config: &RemoteHostConfig) -> Result<(), String> {
    validate_host_config(config)?;
    paths.ensure()?;
    let path = host_config_path(paths);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Could not write remote host config: {error}"))?;
    restrict_file(&temporary)?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("Could not publish remote host config: {error}"))?;
    restrict_file(&path)
}

pub fn configure_host(
    paths: &DaemonPaths,
    listen: &str,
    advertise_url: &str,
    certificate_source: &Path,
    key_source: &Path,
) -> Result<RemoteHostConfig, String> {
    paths.ensure()?;
    let certificate = read_regular_file(certificate_source, 1024 * 1024)?;
    let private_key = read_regular_file(key_source, 1024 * 1024)?;
    let certificate_sha256 = certificate_fingerprint(&certificate)?;
    // Parse before copying so a typo cannot disable an already-running host.
    let _ = tls_config_from_pem(&certificate, &private_key)?;
    let certificate_path = paths.root.join("remote-server-cert.pem");
    let private_key_path = paths.root.join("remote-server-key.pem");
    atomic_protected_write(&certificate_path, &certificate)?;
    atomic_protected_write(&private_key_path, &private_key)?;
    let existing = load_host_config(paths)?;
    let runner_id = existing
        .map(|value| value.runner_id)
        .unwrap_or_else(|| format!("runner-{}", uuid::Uuid::new_v4().simple()));
    let config = RemoteHostConfig {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        runner_id,
        listen: listen.to_string(),
        advertise_url: advertise_url.trim_end_matches('/').to_string(),
        certificate_path: certificate_path.to_string_lossy().to_string(),
        private_key_path: private_key_path.to_string_lossy().to_string(),
        certificate_sha256,
        enabled: true,
    };
    save_host_config(paths, &config)?;
    Ok(config)
}

pub async fn spawn_if_configured(
    paths: DaemonPaths,
    desktop: Arc<DesktopControlRuntime>,
    mobile_chat: Arc<dyn super::api::MobileChatQueue>,
    placement: Arc<dyn super::api::PlacementQueue>,
    peer_runs: Arc<dyn crate::daemon::channel_worker::RunQueue>,
) -> Result<bool, String> {
    let Some(config) = load_host_config(&paths)? else {
        return Ok(false);
    };
    if !config.enabled {
        return Ok(false);
    }
    let listener = bind(&config).await?;
    let acceptor = acceptor(&config)?;
    let api = RemoteApi::production(
        paths,
        config.clone(),
        desktop,
        mobile_chat,
        placement,
        peer_runs,
    )?;
    tokio::spawn(async move {
        if let Err(error) = serve_bound(listener, acceptor, api).await {
            eprintln!("remote runner listener stopped: {error}");
        }
    });
    Ok(true)
}

pub async fn serve(
    paths: DaemonPaths,
    desktop: Arc<DesktopControlRuntime>,
    mobile_chat: Arc<dyn super::api::MobileChatQueue>,
    placement: Arc<dyn super::api::PlacementQueue>,
    peer_runs: Arc<dyn crate::daemon::channel_worker::RunQueue>,
) -> Result<(), String> {
    let config =
        load_host_config(&paths)?.ok_or_else(|| "Remote host is not configured".to_string())?;
    if !config.enabled {
        return Err("Remote host is disabled".to_string());
    }
    let listener = bind(&config).await?;
    let acceptor = acceptor(&config)?;
    let api = RemoteApi::production(paths, config, desktop, mobile_chat, placement, peer_runs)?;
    serve_bound(listener, acceptor, api).await
}

async fn bind(config: &RemoteHostConfig) -> Result<TcpListener, String> {
    let address: SocketAddr = config
        .listen
        .parse()
        .map_err(|_| format!("Invalid remote listen address '{}'", config.listen))?;
    TcpListener::bind(address)
        .await
        .map_err(|error| format!("Could not bind remote runner at {address}: {error}"))
}

fn acceptor(config: &RemoteHostConfig) -> Result<TlsAcceptor, String> {
    let certificate = std::fs::read(&config.certificate_path)
        .map_err(|error| format!("Could not read remote TLS certificate: {error}"))?;
    if certificate_fingerprint(&certificate)? != config.certificate_sha256 {
        return Err("Remote TLS certificate no longer matches its configured pin".to_string());
    }
    let private_key = std::fs::read(&config.private_key_path)
        .map_err(|error| format!("Could not read remote TLS private key: {error}"))?;
    Ok(TlsAcceptor::from(Arc::new(tls_config_from_pem(
        &certificate,
        &private_key,
    )?)))
}

async fn serve_bound(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    api: RemoteApi,
) -> Result<(), String> {
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|error| format!("Remote runner accept failed: {error}"))?;
        let acceptor = acceptor.clone();
        let api = api.clone();
        tokio::spawn(async move {
            let result = async {
                let tls = acceptor
                    .accept(stream)
                    .await
                    .map_err(|error| format!("TLS handshake from {peer} failed: {error}"))?;
                http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(
                        TokioIo::new(tls),
                        service_fn(move |request| handle_http(api.clone(), request)),
                    )
                    .await
                    .map_err(|error| format!("Remote HTTP connection failed: {error}"))
            }
            .await;
            if let Err(error) = result {
                eprintln!("remote runner: {error}");
            }
        });
    }
}

async fn handle_http(
    api: RemoteApi,
    mut request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().as_str().to_ascii_uppercase();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    if let Some(response) = super::web::asset(&method, &path_and_query) {
        return Ok(to_http(response));
    }
    // Checked before the body is collected, because there is no body: this is
    // the one route on the plane that becomes a socket instead of answering.
    if method == "GET" {
        if let Some(upgrade) = talk_upgrade(&api, &mut request, &path_and_query) {
            return Ok(upgrade);
        }
    }
    let auth = parse_auth(request.headers());
    let body = match Limited::new(request.into_body(), MAX_REMOTE_BODY_BYTES)
        .collect()
        .await
    {
        Ok(value) => value.to_bytes().to_vec(),
        Err(_) => {
            return Ok(to_http(ApiResponse::error(
                413,
                "Request body is too large",
            )))
        }
    };
    // `handle_waiting`, not `handle`: identical for every route except the
    // device command lease, which is allowed to hold this connection for its
    // bounded long poll instead of making phones poll on a timer.
    let response = api
        .handle_waiting(
            ApiRequest {
                method,
                path_and_query,
                body,
                auth: auth.ok(),
            },
            now_ms(),
        )
        .await;
    Ok(to_http(response))
}

fn parse_auth(headers: &hyper::HeaderMap) -> Result<SignedRequestHeaders, String> {
    Ok(SignedRequestHeaders {
        device_id: header(headers, HEADER_DEVICE)?.to_string(),
        secret_generation: header(headers, HEADER_GENERATION)?
            .parse()
            .map_err(|_| "Invalid key generation header".to_string())?,
        sequence: header(headers, HEADER_SEQUENCE)?
            .parse()
            .map_err(|_| "Invalid sequence header".to_string())?,
        timestamp_ms: header(headers, HEADER_TIMESTAMP)?
            .parse()
            .map_err(|_| "Invalid timestamp header".to_string())?,
        nonce: header(headers, HEADER_NONCE)?.to_string(),
        command_id: header(headers, HEADER_COMMAND)?.to_string(),
        signature: header(headers, HEADER_SIGNATURE)?.to_string(),
    })
}

fn header<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Result<&'a str, String> {
    headers
        .get(name)
        .ok_or_else(|| format!("Missing {name} header"))?
        .to_str()
        .map_err(|_| format!("Invalid {name} header"))
}

// --- The Talk WebSocket ----------------------------------------------------
//
// **Why the handshake is written out here rather than delegated.** The one
// thing a WebSocket upgrade must not do is bypass the checks every other route
// on this plane passes. So the sequence is deliberate and short: recognise the
// path, refuse anything that is not a real upgrade, spend the one-use ticket
// *before* answering 101, and only then hand the connection over. A socket that
// reaches `run_talk_session` has already proven, through the signed request that
// minted its ticket, exactly what a signed request proves.

/// The session id and ticket of a Talk stream request, if this is one.
///
/// The ticket is a query parameter rather than a path segment on purpose: it
/// keeps the bearer out of anything that treats a path as an identifier.
fn talk_stream_target(path_and_query: &str) -> Option<(String, String)> {
    let (path, query) = path_and_query.split_once('?')?;
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let ["v1", "remote", "device", "talk", session_id, "stream"] = segments.as_slice() else {
        return None;
    };
    let ticket = query.split('&').find_map(|pair| {
        pair.strip_prefix("ticket=")
            .map(|value| percent_decode(value))
    })?;
    Some((percent_decode(session_id), ticket))
}

/// Minimal, allocation-bounded percent decoding for the two values above. Both
/// are validated as strict identifiers/tokens downstream, so anything this
/// leaves malformed is refused there rather than here.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header_contains(headers: &hyper::HeaderMap, name: &str, needle: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(needle))
        })
}

/// RFC 6455's `Sec-WebSocket-Accept`. SHA-1 here is protocol, not security —
/// it authenticates nothing, and the ticket above is what does.
fn websocket_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{GUID}").as_bytes(),
    );
    STANDARD.encode(digest.as_ref())
}

fn talk_upgrade(
    api: &RemoteApi,
    request: &mut Request<Incoming>,
    path_and_query: &str,
) -> Option<Response<Full<Bytes>>> {
    let (session_id, ticket) = talk_stream_target(path_and_query)?;
    if !header_contains(request.headers(), "upgrade", "websocket")
        || !header_contains(request.headers(), "connection", "upgrade")
    {
        // Not an upgrade: fall through to the ordinary signed dispatch, which
        // answers 426 and says how to open one properly.
        return None;
    }
    let key = request
        .headers()
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let Some(key) = key else {
        return Some(to_http(ApiResponse::error(
            400,
            "A WebSocket upgrade needs a Sec-WebSocket-Key",
        )));
    };
    if request
        .headers()
        .get("sec-websocket-version")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        != Some("13")
    {
        return Some(to_http(ApiResponse::error(
            400,
            "Only WebSocket version 13 is supported",
        )));
    }
    // Spent before the 101 is written, under the API's own lock: two sockets
    // racing with one ticket means exactly one of them is admitted.
    let Some(authorization) = api.consume_talk_ticket(&session_id, &ticket, now_ms()) else {
        return Some(to_http(ApiResponse::error(
            401,
            "That Talk ticket is unknown, expired, or already spent",
        )));
    };
    let upgraded = hyper::upgrade::on(request);
    let api = api.clone();
    tokio::spawn(async move {
        match upgraded.await {
            Ok(connection) => {
                super::talk_socket::serve(api, authorization, TokioIo::new(connection)).await;
            }
            Err(error) => eprintln!("remote runner: Talk upgrade failed: {error}"),
        }
    });
    Some(
        Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(hyper::header::UPGRADE, "websocket")
            .header("sec-websocket-accept", websocket_accept(&key))
            .body(Full::new(Bytes::new()))
            .expect("static upgrade response is valid"),
    )
}

fn to_http(value: ApiResponse) -> Response<Full<Bytes>> {
    // The controller document gets a policy that permits what it implements;
    // everything else keeps the deny-everything one. A single header for both
    // was the bug: it denied this page the camera, microphone, location and
    // screen capture it exists to reach, so the preparation controls could not
    // work however the browser's own permission prompt was answered.
    let document = super::web::is_controller_document(value.content_type);
    let (permissions, csp) = if document {
        (
            super::web::CONTROLLER_PERMISSIONS_POLICY,
            super::web::CONTROLLER_CSP,
        )
    } else {
        (super::web::API_PERMISSIONS_POLICY, super::web::API_CSP)
    };
    Response::builder()
        .status(StatusCode::from_u16(value.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(CONTENT_TYPE, value.content_type)
        .header(CACHE_CONTROL, "no-store")
        .header("pragma", "no-cache")
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .header("permissions-policy", permissions)
        .header("cross-origin-opener-policy", "same-origin")
        .header("cross-origin-resource-policy", "same-origin")
        .header("content-security-policy", csp)
        .body(Full::new(Bytes::from(value.body)))
        .expect("static remote response is valid")
}

fn tls_config_from_pem(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<rustls::ServerConfig, String> {
    let certificates = pem_blocks(certificate_pem, "CERTIFICATE")?
        .into_iter()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    let key = if let Ok(values) = pem_blocks(private_key_pem, "PRIVATE KEY") {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            values
                .into_iter()
                .next()
                .ok_or_else(|| "TLS private key is empty".to_string())?,
        ))
    } else if let Ok(values) = pem_blocks(private_key_pem, "RSA PRIVATE KEY") {
        PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(
            values
                .into_iter()
                .next()
                .ok_or_else(|| "TLS private key is empty".to_string())?,
        ))
    } else {
        let values = pem_blocks(private_key_pem, "EC PRIVATE KEY")?;
        PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(
            values
                .into_iter()
                .next()
                .ok_or_else(|| "TLS private key is empty".to_string())?,
        ))
    };
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| format!("TLS certificate/key do not form a usable identity: {error}"))
}

fn pem_blocks(bytes: &[u8], label: &str) -> Result<Vec<Vec<u8>>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "PEM is not UTF-8".to_string())?;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut rest = text;
    let mut output = Vec::new();
    while let Some(start) = rest.find(&begin) {
        rest = &rest[start + begin.len()..];
        let finish = rest
            .find(&end)
            .ok_or_else(|| format!("PEM block '{label}' is incomplete"))?;
        let encoded = rest[..finish]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        output.push(
            STANDARD
                .decode(encoded)
                .map_err(|error| format!("PEM block '{label}' is invalid: {error}"))?,
        );
        rest = &rest[finish + end.len()..];
    }
    if output.is_empty() {
        Err(format!("PEM block '{label}' is missing"))
    } else {
        Ok(output)
    }
}

fn validate_host_config(config: &RemoteHostConfig) -> Result<(), String> {
    if config.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err("Unsupported remote host protocol version".to_string());
    }
    super::protocol::validate_id(&config.runner_id)?;
    config
        .listen
        .parse::<SocketAddr>()
        .map_err(|_| "Remote listen address must be IP:port".to_string())?;
    let url = url::Url::parse(&config.advertise_url)
        .map_err(|error| format!("Invalid advertised remote URL: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Advertised remote URL must be a credential-free HTTPS origin".to_string());
    }
    super::protocol::validate_sha256(&config.certificate_sha256)?;
    Ok(())
}

fn read_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect '{}': {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Remote TLS input '{}' must be a regular non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "Remote TLS input '{}' is too large",
            path.display()
        ));
    }
    std::fs::read(path).map_err(|error| format!("Could not read '{}': {error}", path.display()))
}

fn atomic_protected_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| !metadata.file_type().is_file() || metadata.file_type().is_symlink())
    {
        return Err(format!(
            "Refusing to replace unsafe path '{}'",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create '{}': {error}", temporary.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("Could not write '{}': {error}", temporary.display()))?;
    file.sync_all()
        .map_err(|error| format!("Could not sync '{}': {error}", temporary.display()))?;
    restrict_file(&temporary)?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("Could not publish '{}': {error}", path.display()))?;
    restrict_file(path)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the Talk stream path, and only with a ticket, is a candidate for an
    /// upgrade. Everything else falls through to the ordinary signed dispatch —
    /// which is what stops the upgrade branch from becoming a second, weaker
    /// way into the plane.
    #[test]
    fn only_a_ticketed_talk_stream_path_is_a_websocket_candidate() {
        assert_eq!(
            talk_stream_target("/v1/remote/device/talk/session-one/stream?ticket=abc123"),
            Some(("session-one".to_string(), "abc123".to_string()))
        );
        // The ticket may be any query parameter position, and is decoded.
        assert_eq!(
            talk_stream_target("/v1/remote/device/talk/s1/stream?x=1&ticket=a%2Db"),
            Some(("s1".to_string(), "a-b".to_string()))
        );
        for refused in [
            // No ticket at all.
            "/v1/remote/device/talk/session-one/stream",
            "/v1/remote/device/talk/session-one/stream?other=1",
            // Not the stream route.
            "/v1/remote/device/talk/ticket?ticket=abc",
            "/v1/remote/runs?ticket=abc",
            // Neighbouring shapes that must not be mistaken for it.
            "/v1/remote/device/talk/session-one/stream/extra?ticket=abc",
            "/v1/remote/device/voice/session-one/chunk?ticket=abc",
        ] {
            assert!(talk_stream_target(refused).is_none(), "{refused}");
        }
    }

    /// RFC 6455's example handshake, so a browser that follows the spec is
    /// answered correctly rather than "it worked on the one client I tried".
    #[test]
    fn the_handshake_answers_the_key_the_specification_defines() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// `Connection: keep-alive, Upgrade` is what real clients send; a check
    /// that compared the whole header would refuse them.
    #[test]
    fn upgrade_headers_are_read_as_lists_rather_than_as_whole_values() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        assert!(header_contains(&headers, "connection", "upgrade"));
        assert!(header_contains(&headers, "upgrade", "websocket"));
        assert!(!header_contains(&headers, "upgrade", "h2c"));

        let mut plain = hyper::HeaderMap::new();
        plain.insert("connection", "keep-alive".parse().unwrap());
        assert!(!header_contains(&plain, "connection", "upgrade"));
    }

    #[test]
    fn advertised_url_rejects_http_credentials_query_and_fragment() {
        let base = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:48321".into(),
            advertise_url: "https://runner.test".into(),
            certificate_path: "/cert".into(),
            private_key_path: "/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        assert!(validate_host_config(&base).is_ok());
        for invalid in [
            "http://runner.test",
            "https://user:pass@runner.test",
            "https://runner.test/base",
            "https://runner.test?secret=x",
            "https://runner.test/#fragment",
        ] {
            let mut value = base.clone();
            value.advertise_url = invalid.into();
            assert!(validate_host_config(&value).is_err(), "{invalid}");
        }
    }

    #[test]
    fn auth_parser_fails_closed_when_any_header_is_missing() {
        let headers = hyper::HeaderMap::new();
        assert!(parse_auth(&headers).is_err());
    }

    #[test]
    fn controller_response_has_strict_browser_security_headers() {
        let response = to_http(super::super::web::asset("GET", "/remote").unwrap());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        let policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("connect-src 'self'"));
        assert!(policy.contains("frame-ancestors 'none'"));

        // The microphone is open to this page and to nothing else. Both halves
        // matter: `()` would deny Talk and `voice_stream` outright, and `*`
        // would hand the microphone to anything this page ever embeds.
        let permissions = response.headers()["permissions-policy"].to_str().unwrap();
        assert!(permissions.contains("microphone=(self)"));
        assert!(permissions.contains("camera=()"));
        assert!(permissions.contains("geolocation=()"));
        assert!(!permissions.contains("microphone=*"));
    }

    /// The controller must be permitted to use the hardware it implements.
    ///
    /// This is the assertion whose absence let every device test pass while the
    /// real browser path was dead: an empty allowlist — `camera=()` — disables
    /// the feature for the document, so `getUserMedia`, `getDisplayMedia` and
    /// `getCurrentPosition` were refused before any permission prompt could
    /// appear, and no amount of correct client logic could reach hardware.
    #[test]
    fn the_controller_document_is_permitted_to_use_what_it_implements() {
        let response = to_http(super::super::web::asset("GET", "/remote").unwrap());
        let policy = response.headers()["permissions-policy"].to_str().unwrap();
        for feature in ["camera", "microphone", "geolocation", "display-capture"] {
            assert!(
                policy.contains(&format!("{feature}=(self)")),
                "the controller implements {feature} and this policy forbids it: {policy}"
            );
            assert!(
                !policy.contains(&format!("{feature}=()")),
                "an empty allowlist disables {feature} outright: {policy}"
            );
        }
        // Narrow, not open: this origin only, and nothing it does not use.
        assert!(
            !policy.contains('*'),
            "the allowlist must name self, not all"
        );
        for denied in ["payment=()", "usb=()"] {
            assert!(
                policy.contains(denied),
                "{denied} must stay denied: {policy}"
            );
        }
    }

    /// …and nothing else is. A signed API response has no reason to reach a
    /// camera, so it keeps the deny-everything policy.
    #[test]
    fn an_api_response_still_denies_every_hardware_feature() {
        let response = to_http(ApiResponse::error(401, "nope"));
        let policy = response.headers()["permissions-policy"].to_str().unwrap();
        for feature in ["camera", "microphone", "geolocation", "display-capture"] {
            assert!(
                policy.contains(&format!("{feature}=()")),
                "{feature} must stay denied outside the controller document: {policy}"
            );
        }
    }

    /// The audio the controller plays has to be allowed to load.
    ///
    /// `media-src` has no fallback but `default-src`, which is `'none'` here, so
    /// without it the two real audio sources — a `blob:` URL for an artifact and
    /// a `data:` URL for the silence that unlocks autoplay — were both refused.
    /// The header and the document's own `<meta>` copy are both enforced and the
    /// browser intersects them, so they are checked together: a directive
    /// present in one and missing from the other still blocks.
    #[test]
    fn the_controller_may_load_the_audio_it_plays_under_both_policy_copies() {
        let response = to_http(super::super::web::asset("GET", "/remote").unwrap());
        let header = response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        let html = String::from_utf8(super::super::web::asset("GET", "/remote").unwrap().body)
            .expect("the controller document is text");
        let meta = html
            .split_once("http-equiv=\"Content-Security-Policy\" content=\"")
            .and_then(|(_, tail)| tail.split_once('"'))
            .map(|(value, _)| value.to_string())
            .expect("the controller document still declares its own policy");
        assert_eq!(
            header, meta,
            "both copies are enforced and the browser intersects them; they must not drift"
        );
        for policy in [&header, &meta] {
            assert!(policy.contains("media-src 'self' blob: data:"), "{policy}");
            // The strictness the relaxation must not have cost.
            assert!(policy.contains("default-src 'none'"), "{policy}");
            assert!(policy.contains("object-src 'none'"), "{policy}");
            assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
            assert!(policy.contains("base-uri 'none'"), "{policy}");
            assert!(!policy.contains("unsafe-inline"), "{policy}");
            assert!(!policy.contains("unsafe-eval"), "{policy}");
        }
    }
}
