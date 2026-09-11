//! Ephemeral PCM bridge between paired Talk sockets and the desktop Realtime WebRTC session.
//!
//! This is intentionally not part of the VoiceRoute ledger. Audio exists only in bounded
//! in-memory queues and on a loopback-only HTTP socket authenticated by a process-local token.
//! Route generation is re-checked on every ingress/egress operation so media from a moved or
//! revoked route cannot become current again.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::daemon::store::{restrict_file, DaemonPaths};

use super::api::{RemoteApi, TalkSocketAuthorization};
use super::protocol::validate_id;

pub const REALTIME_PCM_MEDIA_TYPE: &str = "audio/pcm16;rate=24000";
pub const REALTIME_PCM_SAMPLE_RATE_HZ: u32 = 24_000;
pub const REALTIME_PCM_CHANNELS: u8 = 1;
pub const MAX_REALTIME_PCM_CHUNK_BYTES: usize = 64 * 1024;
const MAX_REALTIME_PCM_QUEUE_BYTES: usize = 1024 * 1024;
const MAX_REALTIME_PCM_QUEUE_CHUNKS: usize = 128;
const MAX_REALTIME_ROUTE_QUEUES: usize = 32;
const HOST_MEDIA_PROTOCOL_VERSION: u32 = 1;
const HOST_MEDIA_TOKEN_HEADER: &str = "x-little-monkey-host-media-token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealtimePcmChunk {
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct DirectionQueue {
    chunks: VecDeque<RealtimePcmChunk>,
    bytes: usize,
    next_sequence: u64,
}

impl DirectionQueue {
    fn push(&mut self, bytes: Vec<u8>) -> Result<u64, String> {
        validate_pcm(&bytes)?;
        if self.chunks.len() >= MAX_REALTIME_PCM_QUEUE_CHUNKS
            || self.bytes.saturating_add(bytes.len()) > MAX_REALTIME_PCM_QUEUE_BYTES
        {
            return Err("Realtime media bridge backpressure limit reached".to_string());
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        let sequence = self.next_sequence;
        self.bytes = self.bytes.saturating_add(bytes.len());
        self.chunks.push_back(RealtimePcmChunk { sequence, bytes });
        Ok(sequence)
    }

    fn pop(&mut self) -> Option<RealtimePcmChunk> {
        let chunk = self.chunks.pop_front()?;
        self.bytes = self.bytes.saturating_sub(chunk.bytes.len());
        Some(chunk)
    }
}

#[derive(Default)]
struct RouteQueue {
    input: DirectionQueue,
    output: DirectionQueue,
    last_touched: u64,
}

#[derive(Default)]
pub(crate) struct RealtimeMediaBridge {
    routes: Mutex<HashMap<(String, u64), RouteQueue>>,
}

impl RealtimeMediaBridge {
    fn queue_mut<'a>(
        routes: &'a mut HashMap<(String, u64), RouteQueue>,
        session_id: &str,
        generation: u64,
    ) -> &'a mut RouteQueue {
        if !routes.contains_key(&(session_id.to_string(), generation))
            && routes.len() >= MAX_REALTIME_ROUTE_QUEUES
        {
            if let Some(oldest) = routes
                .iter()
                .min_by_key(|(_, queue)| queue.last_touched)
                .map(|(key, _)| key.clone())
            {
                routes.remove(&oldest);
            }
        }
        routes
            .entry((session_id.to_string(), generation))
            .or_default()
    }

    pub(crate) fn push_input(
        &self,
        session_id: &str,
        generation: u64,
        bytes: Vec<u8>,
    ) -> Result<u64, String> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "Realtime media bridge lock was poisoned".to_string())?;
        let queue = Self::queue_mut(&mut routes, session_id, generation);
        queue.last_touched = monotonic_tick();
        queue.input.push(bytes)
    }

    pub(crate) fn pop_input(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<Option<RealtimePcmChunk>, String> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "Realtime media bridge lock was poisoned".to_string())?;
        let Some(queue) = routes.get_mut(&(session_id.to_string(), generation)) else {
            return Ok(None);
        };
        queue.last_touched = monotonic_tick();
        Ok(queue.input.pop())
    }

    pub(crate) fn push_output(
        &self,
        session_id: &str,
        generation: u64,
        bytes: Vec<u8>,
    ) -> Result<u64, String> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "Realtime media bridge lock was poisoned".to_string())?;
        let queue = Self::queue_mut(&mut routes, session_id, generation);
        queue.last_touched = monotonic_tick();
        queue.output.push(bytes)
    }

    pub(crate) fn pop_output(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<Option<RealtimePcmChunk>, String> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "Realtime media bridge lock was poisoned".to_string())?;
        let Some(queue) = routes.get_mut(&(session_id.to_string(), generation)) else {
            return Ok(None);
        };
        queue.last_touched = monotonic_tick();
        Ok(queue.output.pop())
    }

    pub(crate) fn clear_output(&self, session_id: &str, generation: u64) -> Result<(), String> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "Realtime media bridge lock was poisoned".to_string())?;
        let Some(queue) = routes.get_mut(&(session_id.to_string(), generation)) else {
            return Ok(());
        };
        queue.output.chunks.clear();
        queue.output.bytes = 0;
        queue.last_touched = monotonic_tick();
        Ok(())
    }

    pub(crate) fn discard(&self, session_id: &str, generation: u64) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(&(session_id.to_string(), generation));
        }
    }
}

fn monotonic_tick() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TICK: AtomicU64 = AtomicU64::new(0);
    TICK.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn validate_pcm(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("Realtime PCM chunk is empty".to_string());
    }
    if bytes.len() > MAX_REALTIME_PCM_CHUNK_BYTES {
        return Err(format!(
            "Realtime PCM chunk exceeds {MAX_REALTIME_PCM_CHUNK_BYTES} bytes"
        ));
    }
    if bytes.len() % 2 != 0 {
        return Err("Realtime PCM16 chunk has an incomplete sample".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostMediaConfig {
    protocol_version: u32,
    listen: String,
    token: String,
}

fn config_path(paths: &DaemonPaths) -> std::path::PathBuf {
    paths.root.join("realtime-host-media.json")
}

fn save_config(paths: &DaemonPaths, config: &HostMediaConfig) -> Result<(), String> {
    paths.ensure()?;
    let path = config_path(paths);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Could not write Realtime host-media config: {error}"))?;
    restrict_file(&temporary)?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("Could not publish Realtime host-media config: {error}"))?;
    restrict_file(&path)
}

fn fresh_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Could not generate Realtime host-media token".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Starts a loopback-only transport used by the desktop process to exchange raw PCM with
/// the daemon-owned paired Talk sockets. The token is rewritten on every daemon start.
pub(crate) async fn spawn_host_media_bridge(
    paths: &DaemonPaths,
    api: RemoteApi,
) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("Could not bind Realtime host-media bridge: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Could not read Realtime host-media address: {error}"))?;
    let token = fresh_token()?;
    save_config(
        paths,
        &HostMediaConfig {
            protocol_version: HOST_MEDIA_PROTOCOL_VERSION,
            listen: address.to_string(),
            token: token.clone(),
        },
    )?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                break;
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let api = api.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request| handle_host_media(api.clone(), token.clone(), request)),
                    )
                    .await;
            });
        }
    });
    Ok(address)
}

#[cfg(test)]
pub(crate) async fn spawn_host_media_bridge_for_test(
    paths: &DaemonPaths,
    api: RemoteApi,
) -> Result<SocketAddr, String> {
    spawn_host_media_bridge(paths, api).await
}

async fn handle_host_media(
    api: RemoteApi,
    expected_token: String,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Browser WebViews reach this loopback socket cross-origin. Preflight is
    // intentionally unauthenticated; every media request still needs the
    // process-local 256-bit token, and the listener itself is loopback-only.
    if request.method() == Method::OPTIONS {
        return Ok(host_response(StatusCode::NO_CONTENT, Bytes::new()));
    }
    let supplied = request
        .headers()
        .get(HOST_MEDIA_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if ring::constant_time::verify_slices_are_equal(supplied.as_bytes(), expected_token.as_bytes())
        .is_err()
    {
        return Ok(host_response(StatusCode::UNAUTHORIZED, Bytes::new()));
    }
    let Some((session_id, generation, direction)) = parse_target(request.uri().path()) else {
        return Ok(host_response(StatusCode::NOT_FOUND, Bytes::new()));
    };
    let result = match (request.method(), direction) {
        (&Method::GET, "input") => match api.take_realtime_input_for_host(&session_id, generation) {
            Ok(Some(chunk)) => {
                let mut response = host_response(StatusCode::OK, Bytes::from(chunk.bytes));
                if let Ok(value) = hyper::header::HeaderValue::from_str(&chunk.sequence.to_string()) {
                    response.headers_mut().insert("x-little-monkey-audio-sequence", value);
                }
                response
            }
            Ok(None) => host_response(StatusCode::NO_CONTENT, Bytes::new()),
            Err(_) => host_response(StatusCode::CONFLICT, Bytes::new()),
        },
        (&Method::POST, "output") => {
            let body = match Limited::new(request.into_body(), MAX_REALTIME_PCM_CHUNK_BYTES)
                .collect()
                .await
            {
                Ok(value) => value.to_bytes().to_vec(),
                Err(_) => return Ok(host_response(StatusCode::PAYLOAD_TOO_LARGE, Bytes::new())),
            };
            match api.push_realtime_output_from_host(&session_id, generation, body) {
                Ok(_) => host_response(StatusCode::ACCEPTED, Bytes::new()),
                Err(_) => host_response(StatusCode::CONFLICT, Bytes::new()),
            }
        }
        (&Method::DELETE, "output") => match api.clear_realtime_output_from_host(&session_id, generation) {
            Ok(()) => host_response(StatusCode::NO_CONTENT, Bytes::new()),
            Err(_) => host_response(StatusCode::CONFLICT, Bytes::new()),
        },
        _ => host_response(StatusCode::METHOD_NOT_ALLOWED, Bytes::new()),
    };
    Ok(result)
}

fn host_response(status: StatusCode, body: Bytes) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .header("content-type", "application/octet-stream")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, POST, DELETE, OPTIONS")
        .header(
            "access-control-allow-headers",
            "content-type, x-little-monkey-host-media-token",
        )
        .header("access-control-expose-headers", "x-little-monkey-audio-sequence")
        .body(Full::new(body))
        .expect("static host-media response is valid")
}

fn parse_target(path: &str) -> Option<(String, u64, &'static str)> {
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let ["v1", "host", "realtime", raw_session, raw_generation, direction] = parts.as_slice() else {
        return None;
    };
    let session_id = percent_decode(raw_session);
    validate_id(&session_id).ok()?;
    let generation = raw_generation.parse::<u64>().ok().filter(|value| *value > 0)?;
    let direction = match *direction {
        "input" => "input",
        "output" => "output",
        _ => return None,
    };
    Some((session_id, generation, direction))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_queue_is_bounded_and_pcm_aligned() {
        let bridge = RealtimeMediaBridge::default();
        assert!(bridge.push_input("chat-one", 1, vec![0]).is_err());
        assert!(bridge.push_input("chat-one", 1, vec![0, 1, 2, 3]).is_ok());
        let chunk = bridge.pop_input("chat-one", 1).unwrap().unwrap();
        assert_eq!(chunk.bytes, vec![0, 1, 2, 3]);
        assert_eq!(chunk.sequence, 1);
    }

    #[test]
    fn host_path_is_generation_scoped() {
        assert_eq!(
            parse_target("/v1/host/realtime/chat-one/9/input"),
            Some(("chat-one".to_string(), 9, "input"))
        );
        assert!(parse_target("/v1/host/realtime/chat-one/0/input").is_none());
        assert!(parse_target("/v1/host/realtime/chat-one/9/other").is_none());
    }
}
