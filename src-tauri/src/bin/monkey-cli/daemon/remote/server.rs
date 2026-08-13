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
) -> Result<bool, String> {
    let Some(config) = load_host_config(&paths)? else {
        return Ok(false);
    };
    if !config.enabled {
        return Ok(false);
    }
    let listener = bind(&config).await?;
    let acceptor = acceptor(&config)?;
    let api = RemoteApi::production(paths, config.clone(), desktop, mobile_chat, placement)?;
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
) -> Result<(), String> {
    let config =
        load_host_config(&paths)?.ok_or_else(|| "Remote host is not configured".to_string())?;
    if !config.enabled {
        return Err("Remote host is disabled".to_string());
    }
    let listener = bind(&config).await?;
    let acceptor = acceptor(&config)?;
    let api = RemoteApi::production(paths, config, desktop, mobile_chat, placement)?;
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
    request: Request<Incoming>,
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

fn to_http(value: ApiResponse) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(value.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(CONTENT_TYPE, value.content_type)
        .header(CACHE_CONTROL, "no-store")
        .header("pragma", "no-cache")
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .header(
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), display-capture=(), payment=(), usb=()",
        )
        .header("cross-origin-opener-policy", "same-origin")
        .header("cross-origin-resource-policy", "same-origin")
        .header(
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
        )
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
    }
}
