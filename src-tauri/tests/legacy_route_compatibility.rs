//! D1 pre-merge safety net: byte-level pins on the **legacy** `server.rs`
//! HTTP surface, as it behaves today.
//!
//! D1 wants to collapse two live HTTP servers (`server.rs`, the legacy
//! OpenAI-compatible routing proxy, and `m3_http_server.rs`, the M3 hub's
//! server) into one. The merge is dangerous because the two disagree on
//! details that are invisible to a `body.contains("ok")`-style test and
//! load-bearing to a real SDK client: whether a CORS header is present at
//! all, which keys an error envelope nests and in what order, whether SSE
//! frames are relayed or re-framed. `m3_compatibility_harness.rs` already
//! pins the m3 side's *routes*; nothing pinned legacy's *bytes*. This file
//! is that half — it exists so that a merge which changes one of those
//! bytes fails here, naming the byte, instead of failing in a user's SDK.
//!
//! It deliberately lives in `tests/` rather than in `server.rs`'s own
//! `#[cfg(test)] mod tests`: the whole point of D1 is that `server.rs` (and
//! with it every embedded test that today asserts some of this) goes away.
//! A pin that is deleted by the change it was meant to guard is not a pin.
//!
//! HOW LEGACY IS REACHED. `handle_request` is the `AppHandle`-free core and
//! would be the hermetic, socket-free way to drive these routes — but
//! `ServerDeps::tokens` is a private field, so no external crate can build a
//! `ServerDeps`, and `serve_one_request` is private too. The only public,
//! `AppHandle`-free entry point into the real router is
//! `server::run_cli_server` (what `monkey-cli api-serve` calls), which binds
//! a loopback listener and drives the *same*
//! `serve_with_admission`/`serve_one_request`/`handle_request` path the GUI
//! accept loop does. So these tests bind a real ephemeral port, following
//! `m3_compatibility_harness.rs`'s conventions exactly (ephemeral-port
//! probe, skip-on-`PermissionDenied` sandbox guard, real `reqwest` client,
//! teardown at the end of the test).
//!
//! The production wrapper still uses llama-server/Ollama's conventional
//! loopback endpoints. This harness drives the same common server with
//! explicitly injected ephemeral endpoints, so a developer's real Ollama
//! daemon can neither make a pin skip nor become accidental test input.
//!
//! WHAT IS NOT PINNED HERE, and why: the third `owned_by` family,
//! `"{provider_id}"` from a configured cloud provider, is unreachable from a
//! test. `handle_models`/`handle_chat_completions` gate that branch on
//! `providers::read_key`, which is keychain-only (no env fallback — that's
//! `read_key_with_env`, which `server.rs` does not call), so reaching it
//! would mean writing to the developer's real OS keychain. That gap is
//! recorded rather than faked.

use little_monkey_lib::m3_runtime_hub::{
    M3DownloadTransport, M3HardwareProbe, M3HubConfig, M3HubResult, M3RuntimeHub,
    M3RuntimeHubDependencies, ReqwestM3DownloadTransport, SystemM3Clock,
};
use little_monkey_lib::runtime_adapter::{HardwareSnapshot, PlatformCapabilities};
use little_monkey_lib::server::{
    run_cli_server_with_m3_hub_and_endpoints, save_config_impl, ApiServerConfig, Backend,
    CliRuntimeEndpoints, Scope, TokenEntry,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------
// The pinned bytes. Constants rather than inline literals so a failure
// message and the claim being made read the same.
// ---------------------------------------------------------------------

/// `GET /health`'s entire body. Legacy builds this with
/// `json!({"status":"ok"})`, which is a single key — but m3's liveness
/// payload is a different object, and any client that string-compares this
/// (or hashes it) breaks on so much as an inserted space.
const HEALTH_BODY: &[u8] = br#"{"status":"ok"}"#;

/// `GET /v1/models`' envelope with a `404`-shaped error body spelled out in
/// full, exactly once, so at least one test in this file states the whole
/// claim literally instead of assembling it from a helper.
///
/// Two byte-level facts hide in here. First, `type` is the constant
/// `"invalid_request_error"` on *every* legacy error regardless of status —
/// `403`, `404`, `502` all carry it, and clients branch on `code`, not on
/// `type`. Second, the keys come out **alphabetically** (`code`, `message`,
/// `type`), because `serde_json` is compiled without `preserve_order` here,
/// so its `Map` is a `BTreeMap` and sorts them — not in the source order
/// `error_response`'s `json!` literal is written in. A merge onto typed
/// response structs would emit source order and silently change these bytes.
const NOT_FOUND_BODY: &[u8] =
    br#"{"error":{"code":"not_found","message":"Not Found","type":"invalid_request_error"}}"#;

/// `GET /v1/models` with only a ready local model visible.
///
/// Pins three things at once: the envelope's own alphabetical key order
/// (`data` before `object`, again not source order), each entry's three keys
/// and their order, and the `owned_by` literal `"local"` that clients filter
/// on. The id is whatever the post-auth lazy catalog read from the upstream —
/// in the GUI loop the same slot is filled from the model file's stem — so the
/// pinned part is the shape and the `owned_by` value, not the name.
const MODELS_LOCAL_ONLY_BODY: &[u8] =
    br#"{"data":[{"id":"pinned-local-model","object":"model","owned_by":"local"}],"object":"list"}"#;

/// `GET /v1/models` with Ollama exposed as well: the local entry first, then
/// one Ollama tag with `owned_by` exactly `"ollama"`. Ordering is part of the
/// pin — legacy appends sections in a fixed sequence (local, Ollama,
/// providers) and a client rendering a list sees that order.
const MODELS_LOCAL_AND_OLLAMA_BODY: &[u8] = br#"{"data":[{"id":"pinned-local-model","object":"model","owned_by":"local"},{"id":"pinned-ollama-tag:latest","object":"model","owned_by":"ollama"}],"object":"list"}"#;

/// The SSE bytes the fake upstream emits, chosen to be *wrong* in several
/// ways a re-framing proxy would quietly correct:
///
///   * a leading comment line (`: keep-alive`), which carries no data,
///   * `data:` with no space after the colon,
///   * one event terminated with CRLF while the rest use LF,
///   * a raw `0xFF` byte, which is not valid UTF-8 at all,
///   * `[DONE]` twice,
///   * and no trailing newline after the last one.
///
/// Legacy relays `upstream.bytes_stream()` frame for frame and never parses
/// it, so every one of these survives. m3 re-frames its own events from a
/// canonical stream, which cannot reproduce any of them. If the merged
/// server ever round-trips SSE through a parser, this const stops matching.
const UPSTREAM_SSE_BYTES: &[u8] = b": keep-alive\n\ndata:{\"choices\":[{\"delta\":{\"content\":\"no space after the colon\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"crlf\"}}]}\r\n\r\ndata: \xff\n\ndata: [DONE]\n\ndata: [DONE]";

/// The upstream's own `Content-Type`, deliberately carrying a parameter.
/// Legacy echoes this header value verbatim rather than substituting a
/// canonical `text/event-stream`.
const UPSTREAM_SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";

/// What the embeddings upstream puts on the wire, written to be *wrong* in
/// several ways a handler that deserialized and re-serialized would silently
/// correct:
///
///   * `object` before `data`, and `index` before `embedding` — source order,
///     not the alphabetical order this crate's `serde_json` `Map` (a
///     `BTreeMap`, no `preserve_order`) would re-emit,
///   * a vendor key legacy has never heard of (`x_upstream_note`), which a
///     typed round-trip through an OpenAI-shaped struct would drop,
///   * `1.0` rather than `1`, which `serde_json` re-serializes as `1.0` but a
///     typed `f32`/`f64` round-trip through some encoders renders differently.
///
/// Legacy never parses this at all: `handle_embeddings` hands the upstream's
/// `bytes()` straight to the response body. m3's embeddings handler builds its
/// own canonical envelope and cannot reproduce any of the above, so this const
/// is what a "cleanup" that pointed `/v1/embeddings` at the m3 handler breaks.
const UPSTREAM_EMBEDDINGS_BODY: &[u8] = br#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[1.0,-0.5]}],"model":"pinned-ollama-tag:latest","x_upstream_note":"relayed verbatim"}"#;

/// The embeddings upstream's own `Content-Type`, deliberately *not* JSON.
/// Unlike the streaming chat path (which echoes the upstream header verbatim),
/// `handle_embeddings` always stamps `application/json` on the way out — so
/// this value must never appear on the client's response.
const UPSTREAM_EMBEDDINGS_CONTENT_TYPE: &str = "application/x-upstream-quirk";

/// The legacy scope refusal for `/v1/embeddings`, spelled out because it is
/// one of the strings that differs between the two implementations: legacy
/// names the route in backticks and uses `code: "insufficient_scope"`, while
/// m3 answers `{"error":{"code":"forbidden",...}}` with "token does not grant
/// the requested scope". A client branching on either string sees a different
/// one depending on which handler owns the route.
const EMBEDDINGS_SCOPE_ERROR_MESSAGE: &str = "This token isn't scoped for `embeddings`.";

/// Bounds the wait for `run_cli_server`'s listener, and every request. A
/// hang would otherwise be indistinguishable from a slow machine: legacy's
/// `local_client` has no timeout of its own, so an upstream that accepts and
/// never answers would wedge the whole test.
const REQUEST_BUDGET: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

fn next_test_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Same ephemeral-port probe as `m3_compatibility_harness.rs`: bind, keep the
/// port, drop the listener so the server under test can rebind it, and treat
/// a sandbox that forbids listeners as "skip", not "fail".
async fn free_loopback_port() -> Option<u16> {
    match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => Some(listener.local_addr().expect("ephemeral address").port()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping legacy route pins: sandbox forbids local listeners");
            None
        }
        Err(error) => panic!("bind ephemeral test port: {error}"),
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_BUDGET)
        .build()
        .expect("loopback client")
}

/// The base config every test starts from: no token required and both
/// optional backends off, so a route's behaviour is a function of the test's
/// own edits and not of whatever the developer's machine happens to be
/// running.
fn base_config() -> ApiServerConfig {
    ApiServerConfig {
        require_token: false,
        expose_ollama: false,
        expose_providers: false,
        ..ApiServerConfig::default()
    }
}

/// Digest of a bearer token's plaintext, in the same lowercase hex form
/// `server.rs`'s private `sha256_hex` produces — that function is what
/// `authenticate` compares against, and it is not reachable from here.
fn token_digest(plaintext: &str) -> String {
    Sha256::digest(plaintext.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct TestHardware;

impl M3HardwareProbe for TestHardware {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        Ok(HardwareSnapshot {
            captured_at_ms: 1_000,
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
            available_ram_bytes: 12 * 1024 * 1024 * 1024,
            logical_cpu_count: 8,
            platform: PlatformCapabilities::from_host("linux", "x86_64", Vec::new()),
        })
    }
}

fn test_m3_hub(root: &std::path::Path) -> Arc<M3RuntimeHub> {
    let download: Arc<dyn M3DownloadTransport> =
        Arc::new(ReqwestM3DownloadTransport::new().expect("test download transport"));
    Arc::new(
        M3RuntimeHub::new(
            root,
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
                lan_factory: None,
            },
        )
        .expect("legacy compatibility test M3 hub"),
    )
}

/// One running legacy server, plus the `api_server.json` it re-reads on
/// **every** request. That re-read is what lets a single test flip
/// `require_token`/`expose_ollama` mid-flight (see `build_deps`' "never
/// stale" note) instead of paying for a second server.
struct LegacyServer {
    base: String,
    data_dir: PathBuf,
    config_path: PathBuf,
    accept_loop: tokio::task::JoinHandle<Result<(), String>>,
}

impl LegacyServer {
    async fn start(label: &str, config: ApiServerConfig) -> Option<Self> {
        Self::start_with_endpoints(label, config, unreachable_runtime_endpoints()).await
    }

    async fn start_with_endpoints(
        label: &str,
        config: ApiServerConfig,
        endpoints: CliRuntimeEndpoints,
    ) -> Option<Self> {
        let port = free_loopback_port().await?;
        let data_dir = std::env::temp_dir().join(format!(
            "legacy-route-compat-{label}-{}-{}",
            std::process::id(),
            next_test_id()
        ));
        std::fs::create_dir(&data_dir).expect("create the private fixture app-data directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
                .expect("make the fixture app-data directory private");
        }
        let config_path = data_dir.join("api_server.json");
        save_config_impl(&config_path, &config).expect("write the legacy server's config");
        let m3_hub = test_m3_hub(&data_dir.join("m3-test-hub"));

        // No custom cloud providers: the built-in presets are catalogued
        // regardless, and none of them can be *routed* to without a keychain
        // key, so the provider branch stays inert either way.
        let accept_loop = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
            port,
            config_path.clone(),
            m3_hub,
            endpoints,
            Vec::new,
        ));

        let mut server = LegacyServer {
            base: format!("http://127.0.0.1:{port}"),
            data_dir,
            config_path,
            accept_loop,
        };

        // `run_cli_server` binds inside the spawned task, so the first
        // request can lose a race with it.
        let client = http_client();
        for _ in 0..100 {
            if client.get(server.url("/health")).send().await.is_ok() {
                return Some(server);
            }
            if server.accept_loop.is_finished() {
                let outcome = (&mut server.accept_loop).await;
                panic!("the legacy server exited before readiness: {outcome:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the legacy server never answered /health — did the port get taken back?");
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn set_config(&self, config: ApiServerConfig) {
        save_config_impl(&self.config_path, &config).expect("rewrite the legacy server's config");
    }
}

fn unreachable_runtime_endpoints() -> CliRuntimeEndpoints {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve an unused test runtime port");
    let port = listener
        .local_addr()
        .expect("unused runtime address")
        .port();
    drop(listener);
    CliRuntimeEndpoints {
        llama_port: port,
        ollama_base_url: format!("http://127.0.0.1:{port}"),
    }
}

impl Drop for LegacyServer {
    fn drop(&mut self) {
        self.accept_loop.abort();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// A raw-TCP stand-in for one of legacy's loopback upstreams. Raw rather
/// than a real server because the SSE pin needs to put bytes on the wire
/// that no well-behaved HTTP library would emit — same technique as
/// `server.rs`'s own embedded upstream fakes and `model_sources.rs`'s tests.
///
/// `respond` receives the request head and returns the complete raw
/// response. The listener is intentionally never closed: it must stay up for
/// the whole test binary because it owns a fixed port that more than one test
/// probes, and the process exiting is what releases it.
fn spawn_fake_upstream(
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> Result<u16, std::io::Error> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).to_string();
            let _ = stream.write_all(&respond(&head));
            let _ = stream.flush();
        }
    });
    Ok(port)
}

fn spawn_counted_fake_upstream(
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> Result<(u16, Arc<AtomicU64>), std::io::Error> {
    let requests = Arc::new(AtomicU64::new(0));
    let observed = requests.clone();
    let port = spawn_fake_upstream(move |head| {
        observed.fetch_add(1, Ordering::SeqCst);
        respond(head)
    })?;
    Ok((port, requests))
}

fn raw_http_response(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Starts isolated ephemeral llama-server and Ollama stand-ins and returns the
/// endpoints injected into the AppHandle-free CLI core. Nothing is skipped:
/// failure to create either loopback fake is a harness failure.
fn fake_runtime_endpoints() -> CliRuntimeEndpoints {
    // llama-server: the post-auth source reads `/v1/models` and takes each
    // `data[].id` as a servable model identity.
    let llama_port = spawn_fake_upstream(|head| {
        if head.starts_with("GET /v1/models") {
            raw_http_response(
                "application/json",
                br#"{"object":"list","data":[{"id":"pinned-local-model","object":"model"}]}"#,
            )
        } else {
            raw_http_response("application/json", br#"{"status":"ok"}"#)
        }
    })
    .expect("bind the ephemeral llama-server stand-in");
    // Ollama: `/api/tags` for model listing and `/v1/chat/completions` for
    // the raw SSE relay.
    let ollama_port = spawn_fake_upstream(|head| {
        if head.starts_with("GET /api/tags") {
            raw_http_response(
                "application/json",
                br#"{"models":[{"name":"pinned-ollama-tag:latest"}]}"#,
            )
        } else {
            raw_http_response(UPSTREAM_SSE_CONTENT_TYPE, UPSTREAM_SSE_BYTES)
        }
    })
    .expect("bind the ephemeral Ollama stand-in");
    CliRuntimeEndpoints {
        llama_port,
        ollama_base_url: format!("http://127.0.0.1:{ollama_port}"),
    }
}

/// Asserts on bytes, and prints both sides as text when they differ — the
/// failure message is the whole product here, since "a byte changed" is only
/// actionable if you can see which one.
#[track_caller]
fn assert_bytes(label: &str, actual: &[u8], expected: &[u8]) {
    assert_eq!(
        actual,
        expected,
        "{label}: legacy's bytes changed\n     actual: {}\n   expected: {}",
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(expected)
    );
}

/// How many times `needle` occurs in `haystack`. Byte-level rather than
/// `str::matches` because the SSE pin deliberately relays a byte that is not
/// valid UTF-8, so the body cannot be turned into a `&str` at all.
fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    count_occurrences(haystack, needle) > 0
}

/// The legacy OpenAI error envelope, assembled from the two fields that vary.
/// `NOT_FOUND_BODY` spells the same shape out literally; this exists so the
/// per-status pins below read as "which code, which message" rather than
/// repeating the nesting six times.
fn error_envelope(code: &str, message: &str) -> String {
    format!(
        r#"{{"error":{{"code":"{code}","message":"{message}","type":"invalid_request_error"}}}}"#
    )
}

fn cors_origin(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

// ---------------------------------------------------------------------
// 2. `/health` is byte-exact, and `/v1/contract` answers beside it
// ---------------------------------------------------------------------

/// The two routes legacy answers **before authentication**, pinned together
/// because that is the one property they share and the one a merge can lose.
///
/// `/health` is a liveness probe: a caller has to reach it before it has a
/// token to present. `/v1/contract` (roadmap K19) is the same situation one
/// step earlier — a client negotiates the ABI before it can know whether the
/// credential shape it holds is still the right one — and its body is a pure
/// function of the built binary, so there is nothing in it to gate.
///
/// One server for both, deliberately: every test in this file binds a real
/// listener and they run in parallel, so an extra server is a real cost on a
/// slow CI runner rather than a free assertion.
#[tokio::test]
async fn the_unauthenticated_routes_answer_byte_for_byte_and_publish_the_abi() {
    let Some(server) = LegacyServer::start("health", base_config()).await else {
        return;
    };
    let client = http_client();

    let response = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "/health's content-type is part of what a liveness probe branches on"
    );
    let body = response.bytes().await.expect("/health body");
    assert_bytes("GET /health", &body, HEALTH_BODY);

    // Counter-test: the pinned bytes belong to `GET /health` specifically,
    // not to "any request that mentions /health". `handle_request` matches
    // the method too, so a POST falls through to the 404 envelope — a
    // merged router that answered any method here would pass the assertion
    // above and fail this one.
    let response = client
        .post(server.url("/health"))
        .send()
        .await
        .expect("POST /health");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let body = response.bytes().await.expect("POST /health body");
    assert_bytes("POST /health", &body, NOT_FOUND_BODY);

    // `GET /v1/contract`, unauthenticated, on the same listener.
    let response = client
        .get(server.url("/v1/contract"))
        .send()
        .await
        .expect("GET /v1/contract");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the ABI is negotiated before a credential is presented"
    );
    let body: serde_json::Value = response.json().await.expect("/v1/contract body");
    assert_eq!(
        body["contract_version"],
        little_monkey_lib::contract::CONTRACT_VERSION
    );
    assert_eq!(body["digest"], little_monkey_lib::contract::digest());
    // The manifest travels whole: a client needs no second request and no
    // shipped copy to know which routes and tools this instance has.
    assert_eq!(
        body["manifest"],
        serde_json::to_value(little_monkey_lib::contract::manifest()).expect("manifest as JSON")
    );
    let published: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../contract/agent-os-contract.json"),
        )
        .expect("the published contract artifact"),
    )
    .expect("published contract JSON");
    assert_eq!(
        body["manifest"], published,
        "a running instance and the published artifact must not disagree"
    );

    // Method-scoped, exactly as `/health` is above.
    let response = client
        .post(server.url("/v1/contract"))
        .send()
        .await
        .expect("POST /v1/contract");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the pre-auth answer belongs to GET, exactly as it does for /health"
    );
}

// ---------------------------------------------------------------------
// 1. `Access-Control-Allow-Origin: *` on every response
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_legacy_response_carries_the_wildcard_cors_origin_including_failures() {
    let Some(server) = LegacyServer::start("cors", base_config()).await else {
        return;
    };
    let client = http_client();

    for path in ["/health", "/v1/models", "/v1/nope"] {
        let response = client
            .get(server.url(path))
            .send()
            .await
            .unwrap_or_else(|error| panic!("GET {path}: {error}"));
        assert_eq!(
            cors_origin(&response).as_deref(),
            Some("*"),
            "GET {path} must carry the wildcard CORS origin — m3's default is deny-all, so a \
             merge that adopts it silently breaks every browser client"
        );
    }

    // The header is not conditional on success. `with_cors` wraps the
    // authentication failure too, and a browser needs it there most of all:
    // without it the fetch rejects with an opaque network error instead of
    // surfacing the 401 the server actually sent.
    server.set_config(ApiServerConfig {
        require_token: true,
        ..base_config()
    });
    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET /v1/models with tokens required");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        cors_origin(&response).as_deref(),
        Some("*"),
        "a 401 must still carry the CORS header"
    );
}

#[tokio::test]
async fn the_cors_origin_is_the_literal_wildcard_and_never_the_callers_own_origin() {
    let Some(server) = LegacyServer::start("cors-origin", base_config()).await else {
        return;
    };
    let client = http_client();

    let response = client
        .get(server.url("/v1/models"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .expect("GET /v1/models with an Origin header");
    assert_eq!(
        cors_origin(&response).as_deref(),
        Some("*"),
        "legacy stamps a fixed `*`; it must not start reflecting the request's Origin, which is \
         what a credentialed CORS setup looks like and this server is not one"
    );

    // Counter-test, and the reason the wildcard is defensible at all: an
    // `Origin` header forces authentication *even though this config has
    // `require_token: false`* (see `authenticate`'s doc comment). So `*` is
    // not a blanket accept — the browser path is the one path that can never
    // be unauthenticated. A merge that keeps the wildcard but drops this rule
    // re-opens "any open browser tab can drive /v1/chat/completions", which is
    // a live, credential-spending route.
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a request carrying Origin must be authenticated even when require_token is off"
    );

    // ...and the boundary of that rule: `/health` answers *before*
    // `authenticate` runs at all, so a browser probing liveness still gets a
    // 200. Pinned because it is the one route where the Origin rule
    // deliberately does not apply, and a merge that "fixed" the
    // inconsistency would break every browser health check.
    let response = client
        .get(server.url("/health"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .expect("GET /health with an Origin header");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(cors_origin(&response).as_deref(), Some("*"));
    assert_bytes(
        "GET /health with an Origin header",
        &response.bytes().await.expect("/health body"),
        HEALTH_BODY,
    );
}

// ---------------------------------------------------------------------
// 3. The OpenAI error envelope SDKs branch on
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_error_envelope_nests_exactly_code_message_and_type_under_error() {
    let Some(server) = LegacyServer::start("envelope", base_config()).await else {
        return;
    };
    let client = http_client();

    let response = client
        .get(server.url("/v1/nope"))
        .send()
        .await
        .expect("GET an unrouted path");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let body = response.bytes().await.expect("404 body");
    assert_bytes("GET /v1/nope", &body, NOT_FOUND_BODY);

    // Structural belt-and-braces on top of the byte compare: no fourth key
    // may appear inside `error`, and nothing may appear beside it at the top
    // level. A merged server that *added* a field (`param`, `request_id`)
    // would still satisfy a `contains` check and still break a client that
    // deserializes into a closed struct.
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("the 404 body is JSON");
    let top = parsed.as_object().expect("a JSON object");
    assert_eq!(
        top.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["error"],
        "`error` is the only top-level key"
    );
    let inner = top["error"].as_object().expect("error is an object");
    assert_eq!(
        inner.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["code", "message", "type"],
        "exactly three keys, in this order"
    );
}

#[tokio::test]
async fn each_legacy_failure_keeps_its_own_code_and_message_bytes() {
    let Some(server) = LegacyServer::start("failures", base_config()).await else {
        return;
    };
    let client = http_client();

    // A body that isn't JSON at all. Note `code` here is *also*
    // `invalid_request_error` — the same string the `type` field always
    // carries. That duplication looks like a bug and is what clients see
    // today, so it is pinned rather than tidied.
    let response = client
        .post(server.url("/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("POST an invalid body");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_bytes(
        "invalid JSON body",
        &response.bytes().await.expect("400 body"),
        error_envelope("invalid_request_error", "Invalid JSON body").as_bytes(),
    );

    // A well-formed body with no `model`: OpenAI's own wording, reproduced
    // deliberately, and a `404` rather than the `400` a fresh design would
    // pick. Clients branch on both.
    let response = client
        .post(server.url("/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST with no model");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_bytes(
        "missing model",
        &response.bytes().await.expect("model_not_found body"),
        error_envelope("model_not_found", "you must provide a model parameter").as_bytes(),
    );

    // `require_token` on with nothing minted yet: fails closed, and says so
    // in a message that names where to fix it.
    server.set_config(ApiServerConfig {
        require_token: true,
        ..base_config()
    });
    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET with no tokens configured");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let no_tokens_body = response.bytes().await.expect("401 body");
    assert_bytes(
        "no tokens configured",
        &no_tokens_body,
        error_envelope(
            "invalid_api_key",
            "No API tokens are configured for this server. Create one in Settings > API Server.",
        )
        .as_bytes(),
    );

    // Unknown and explicitly denied-looking paths still pass through the
    // legacy authentication gate. The typed registry must prevent dispatch,
    // but it must not turn either path into a pre-auth route-existence oracle.
    let response = client
        .get(server.url("/v1/tools"))
        .send()
        .await
        .expect("GET a denied-looking path with no configured tokens");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_bytes(
        "denied-looking legacy path authenticates before 404",
        &response.bytes().await.expect("denied-looking 401 body"),
        &no_tokens_body,
    );

    // Host-only desktop routes are deliberately unavailable to the
    // AppHandle-free CLI, but historically they were ordinary unknown paths
    // there and therefore still authenticated before the 404.
    let response = client
        .post(server.url("/v1/knowledge/query"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST a GUI-only route through the CLI listener");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_bytes(
        "GUI-only legacy path authenticates in the CLI",
        &response.bytes().await.expect("GUI-only 401 body"),
        &no_tokens_body,
    );

    // A token exists but this caller's bearer doesn't match it. Same `code`,
    // different message — and the difference is the point: collapsing the two
    // into one generic 401 would leave an operator unable to tell "I never
    // made a token" from "my token is wrong".
    let scoped_token = "lmk-legacy-pin-token";
    server.set_config(ApiServerConfig {
        require_token: true,
        tokens: vec![TokenEntry {
            id: "legacy-pin".to_string(),
            label: "legacy route pins".to_string(),
            sha256: token_digest(scoped_token),
            // Deliberately *not* `Chat`: this token exists to prove the
            // scope refusal below, and `Models` keeps it usable for the
            // "authenticates fine, wrong scope" case.
            scopes: vec![Scope::Models],
            backends: vec![Backend::Local, Backend::Ollama],
            ..TokenEntry::default()
        }],
        ..base_config()
    });
    let response = client
        .get(server.url("/v1/models"))
        .bearer_auth("lmk-not-the-right-token")
        .send()
        .await
        .expect("GET with a wrong bearer");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong_token_body = response.bytes().await.expect("401 body");
    assert_bytes(
        "wrong bearer token",
        &wrong_token_body,
        error_envelope(
            "invalid_api_key",
            "Incorrect API key provided. Find the current one in Little Monkey's Settings > API Server.",
        )
        .as_bytes(),
    );
    assert_ne!(
        no_tokens_body, wrong_token_body,
        "the two 401 reasons must stay distinguishable"
    );

    // A wrong method on a known legacy route follows the same ordering: an
    // invalid bearer gets the ordinary 401, not a pre-auth 405 that reveals
    // the route. With a valid bearer below, the same request reaches the
    // historical 404 fallthrough.
    let response = client
        .get(server.url("/v1/chat/completions"))
        .bearer_auth("lmk-not-the-right-token")
        .send()
        .await
        .expect("GET a legacy POST route with a wrong bearer");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_bytes(
        "legacy wrong-method request authenticates first",
        &response.bytes().await.expect("wrong-method 401 body"),
        &wrong_token_body,
    );

    let response = client
        .get(server.url("/v1/chat/completions"))
        .bearer_auth(scoped_token)
        .send()
        .await
        .expect("GET a legacy POST route with a valid bearer");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_bytes(
        "legacy authenticated wrong method falls through",
        &response.bytes().await.expect("wrong-method 404 body"),
        NOT_FOUND_BODY,
    );

    let response = client
        .post(server.url("/v1/knowledge/query"))
        .bearer_auth(scoped_token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST an unavailable GUI-only route with a valid bearer");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_bytes(
        "GUI-only legacy path falls through after authentication",
        &response.bytes().await.expect("GUI-only 404 body"),
        NOT_FOUND_BODY,
    );

    // Authenticated, but out of scope: `403`, and a distinct `code` clients
    // use to decide "re-pair with more scopes" instead of "re-enter the key".
    let response = client
        .post(server.url("/v1/chat/completions"))
        .bearer_auth(scoped_token)
        .header("content-type", "application/json")
        .body(r#"{"model":"pinned-ollama-tag:latest"}"#)
        .send()
        .await
        .expect("POST with an under-scoped token");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_bytes(
        "under-scoped token",
        &response.bytes().await.expect("403 body"),
        error_envelope("insufficient_scope", "This token isn't scoped for `chat`.").as_bytes(),
    );

    // Counter-test for the whole block: the same token on the route it *is*
    // scoped for succeeds, and a success is not wrapped in the envelope at
    // all. Without this, an implementation that returned `insufficient_scope`
    // for everything would satisfy every assertion above.
    let response = client
        .get(server.url("/v1/models"))
        .bearer_auth(scoped_token)
        .send()
        .await
        .expect("GET with the correctly scoped token");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.bytes().await.expect("models body");
    assert!(
        !body.starts_with(br#"{"error""#),
        "a success must not carry the error envelope: {}",
        String::from_utf8_lossy(&body)
    );
}

// ---------------------------------------------------------------------
// 6. `OPTIONS /v1/*` is a 204 preflight, before authentication
// ---------------------------------------------------------------------

#[tokio::test]
async fn options_on_a_v1_route_is_a_204_preflight_that_needs_no_token() {
    // `require_token` on with zero tokens minted: every authenticated route
    // 401s under this config, which is exactly what makes the 204 below
    // meaningful — the preflight is answered *before* `authenticate` runs.
    let config = ApiServerConfig {
        require_token: true,
        ..base_config()
    };
    let Some(server) = LegacyServer::start("preflight", config).await else {
        return;
    };
    let client = http_client();

    let response = client
        .request(reqwest::Method::OPTIONS, server.url("/v1/chat/completions"))
        .send()
        .await
        .expect("OPTIONS /v1/chat/completions");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    let headers = response.headers().clone();
    for (name, expected) in [
        ("access-control-allow-origin", "*"),
        ("access-control-allow-methods", "GET, POST, OPTIONS"),
        (
            "access-control-allow-headers",
            "Content-Type, Authorization",
        ),
    ] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            Some(expected),
            "{name} is part of the preflight a browser refuses to send the real request without"
        );
    }
    assert!(
        response.bytes().await.expect("preflight body").is_empty(),
        "a 204 preflight carries no body"
    );

    // The wildcard is literal. A path that would be denied for an execution
    // method remains a harmless legacy preflight: it cannot dispatch a tool,
    // but browser clients still receive the byte-compatible 204 contract.
    let response = client
        .request(reqwest::Method::OPTIONS, server.url("/v1/tools"))
        .send()
        .await
        .expect("OPTIONS /v1/tools");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );
    assert!(response.bytes().await.expect("preflight body").is_empty());

    // Counter-test 1: authentication really is on under this config, so the
    // 204 above is a route-specific bypass and not "this server accepts
    // everything".
    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET /v1/models under the same config");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Counter-test 2: the bypass is scoped to `/v1/*`. `OPTIONS /health` matches
    // neither the health route (wrong method) nor the preflight (wrong prefix), so it
    // lands on the 404 envelope.
    //
    // Config restored first, and that is not tidiness: `authenticate` sits *between*
    // the preflight arm and the `_ =>` fallthrough, so under the `require_token: true`
    // config the counter-test above needs, this request is a 401 and never reaches the
    // 404 at all. An earlier version of this test asserted 404 under the preflight
    // config and failed with `left: 401`.
    server.set_config(base_config());
    let response = client
        .request(reqwest::Method::OPTIONS, server.url("/health"))
        .send()
        .await
        .expect("OPTIONS /health");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_bytes(
        "OPTIONS on a non-/v1 path",
        &response.bytes().await.expect("404 body"),
        NOT_FOUND_BODY,
    );
}

// ---------------------------------------------------------------------
// 4. `owned_by` values clients filter on
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_models_listing_labels_owned_by_local_then_ollama_with_byte_exact_entries() {
    let endpoints = fake_runtime_endpoints();
    let Some(server) = LegacyServer::start_with_endpoints("models", base_config(), endpoints).await
    else {
        return;
    };
    let client = http_client();

    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET /v1/models with only the local model");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_bytes(
        "GET /v1/models (local only)",
        &response.bytes().await.expect("models body"),
        MODELS_LOCAL_ONLY_BODY,
    );

    server.set_config(ApiServerConfig {
        expose_ollama: true,
        ..base_config()
    });
    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET /v1/models with Ollama exposed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_bytes(
        "GET /v1/models (local + ollama)",
        &response.bytes().await.expect("models body"),
        MODELS_LOCAL_AND_OLLAMA_BODY,
    );

    // Counter-test: `"ollama"` is not stamped unconditionally. Turning the
    // toggle back off must remove that entry entirely — legacy's rule is
    // "only advertise what will actually serve", and an implementation that
    // always listed every reachable backend would pass the assertion above.
    server.set_config(base_config());
    let response = client
        .get(server.url("/v1/models"))
        .send()
        .await
        .expect("GET /v1/models with Ollama hidden again");
    assert_bytes(
        "GET /v1/models (ollama hidden again)",
        &response.bytes().await.expect("models body"),
        MODELS_LOCAL_ONLY_BODY,
    );
}

#[tokio::test]
async fn model_sources_are_unobserved_before_auth_and_unknown_ids_are_not_guessed_as_ollama() {
    let (llama_port, llama_requests) = spawn_counted_fake_upstream(|_| {
        raw_http_response(
            "application/json",
            br#"{"object":"list","data":[{"id":"pinned-local-model","object":"model"}]}"#,
        )
    })
    .expect("bind counted llama stand-in");
    let (ollama_port, ollama_requests) = spawn_counted_fake_upstream(|_| {
        raw_http_response(
            "application/json",
            br#"{"models":[{"name":"pinned-ollama-tag:latest"}]}"#,
        )
    })
    .expect("bind counted Ollama stand-in");
    let token = "lmk-catalog-ordering-pin";
    let config = ApiServerConfig {
        require_token: true,
        expose_ollama: true,
        tokens: vec![TokenEntry {
            id: "catalog-ordering".to_string(),
            label: "catalog ordering".to_string(),
            sha256: token_digest(token),
            scopes: vec![Scope::Models, Scope::Chat],
            backends: vec![Backend::Local, Backend::Ollama],
            ..TokenEntry::default()
        }],
        ..base_config()
    };
    let endpoints = CliRuntimeEndpoints {
        llama_port,
        ollama_base_url: format!("http://127.0.0.1:{ollama_port}"),
    };
    let Some(server) =
        LegacyServer::start_with_endpoints("catalog-ordering", config, endpoints).await
    else {
        return;
    };
    let client = http_client();

    let denied = client
        .get(server.url("/v1/models"))
        .bearer_auth("lmk-invalid")
        .send()
        .await
        .expect("invalid bearer request");
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(llama_requests.load(Ordering::SeqCst), 0);
    assert_eq!(ollama_requests.load(Ordering::SeqCst), 0);

    let listed = client
        .get(server.url("/v1/models"))
        .bearer_auth(token)
        .send()
        .await
        .expect("authorized union listing");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);
    assert_bytes(
        "authorized union listing",
        &listed.bytes().await.expect("union body"),
        MODELS_LOCAL_AND_OLLAMA_BODY,
    );
    assert_eq!(llama_requests.load(Ordering::SeqCst), 1);
    assert_eq!(ollama_requests.load(Ordering::SeqCst), 1);

    let unknown = client
        .post(server.url("/v1/chat/completions"))
        .bearer_auth(token)
        .json(&serde_json::json!({"model":"not-advertised","messages":[]}))
        .send()
        .await
        .expect("unknown exact model request");
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);
    let body = unknown.text().await.expect("unknown model body");
    assert!(body.contains("not-advertised"));
    assert!(body.contains("legacy-managed-llama"));
    assert!(body.contains("legacy-ollama"));
}

// ---------------------------------------------------------------------
// 5. Raw SSE passthrough
// ---------------------------------------------------------------------

#[tokio::test]
async fn streamed_upstream_sse_bytes_reach_the_client_unmodified() {
    let endpoints = fake_runtime_endpoints();
    let config = ApiServerConfig {
        expose_ollama: true,
        ..base_config()
    };
    let Some(server) = LegacyServer::start_with_endpoints("sse", config, endpoints).await else {
        return;
    };
    let client = http_client();

    let response = client
        .post(server.url("/v1/chat/completions"))
        .header("content-type", "application/json")
        // Any non-empty id that isn't the ready llama stem routes to Ollama,
        // which is where the fake upstream is.
        .body(r#"{"model":"pinned-ollama-tag:latest","stream":true}"#)
        .send()
        .await
        .expect("POST a streaming chat completion");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some(UPSTREAM_SSE_CONTENT_TYPE),
        "legacy echoes the upstream's content-type verbatim, parameters included"
    );
    assert_eq!(
        cors_origin(&response).as_deref(),
        Some("*"),
        "the streaming path is wrapped by `with_cors` too"
    );
    // The admission body delegates `Body::size_hint`, so the streaming body remains
    // unknown-length while a buffered response below keeps its exact length. That
    // makes the two production branches observable without a timing assertion.
    assert!(
        response.headers().get("content-length").is_none(),
        "the streamed branch must stay unknown-length"
    );

    let body = response.bytes().await.expect("SSE body");
    assert_bytes("streamed SSE body", &body, UPSTREAM_SSE_BYTES);

    // Spelled out separately from the byte compare because these are the
    // individual quirks a re-framing implementation would each "fix", and a
    // named assertion says which one broke.
    assert!(
        body.starts_with(b": keep-alive\n\n"),
        "the upstream's comment frame must survive"
    );
    assert!(
        contains_bytes(&body, b"data:{"),
        "a `data:` with no space after the colon must not be normalised"
    );
    assert!(
        body.contains(&0xFF),
        "a byte that isn't valid UTF-8 must survive, which it cannot if the body was reparsed"
    );
    assert!(
        !body.ends_with(b"\n"),
        "legacy must not append a terminator the upstream never sent"
    );
    assert_eq!(
        count_occurrences(&body, b"[DONE]"),
        2,
        "a duplicated sentinel must be relayed twice, not deduplicated"
    );

    // Counter-test: the passthrough is the *streaming* path specifically.
    // Without `stream: true` legacy buffers the body instead, which is a
    // different branch (`upstream.bytes()`), and the two must not be
    // conflated by a merge that streams everything or buffers everything.
    // The fake answers identically either way, so what this pins is that the
    // non-streaming branch is reached and still returns the upstream's own
    // bytes and content-type.
    let response = client
        .post(server.url("/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"pinned-ollama-tag:latest"}"#)
        .send()
        .await
        .expect("POST a non-streaming chat completion");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let expected_content_length = UPSTREAM_SSE_BYTES.len().to_string();
    assert_eq!(
        response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_length.as_str()),
        "the buffered branch must preserve its exact body length"
    );
    assert_bytes(
        "buffered body",
        &response.bytes().await.expect("buffered body"),
        UPSTREAM_SSE_BYTES,
    );
}

// ---------------------------------------------------------------------
// 7. `/v1/embeddings` — the fourth shared route
// ---------------------------------------------------------------------

/// The one shared route (`shared_route_owner`'s four) that had no byte pin at
/// all. Its legacy and m3 implementations diverge in every dimension this file
/// exists to guard — wildcard CORS versus deny-by-default, a verbatim upstream
/// relay versus a re-serialized canonical envelope, and a different scope-error
/// code *and* message — yet the only tests touching it were generic CORS and
/// envelope tests on *other* routes. A cleanup that collapsed `/v1/embeddings`
/// onto the m3 handler would have turned every browser client of it into an
/// opaque CORS failure with nothing going red.
///
/// Reached through Ollama rather than llama-server on purpose: the
/// `AppHandle`-free CLI core hardcodes `llama_embeddings_enabled: false` (it
/// cannot see the GUI's in-memory `--embeddings` flag), so the llama branch is
/// a `501` here and the Ollama branch is the only one that reaches an upstream.
#[tokio::test]
async fn the_embeddings_route_relays_upstream_bytes_with_wildcard_cors_and_its_own_scope_error() {
    // llama-server's inventory still has to answer, because the merged catalog
    // is read before the model id is matched; only the Ollama half is what this
    // request routes to.
    let llama_port = spawn_fake_upstream(|head| {
        if head.starts_with("GET /v1/models") {
            raw_http_response(
                "application/json",
                br#"{"object":"list","data":[{"id":"pinned-local-model","object":"model"}]}"#,
            )
        } else {
            raw_http_response("application/json", br#"{"status":"ok"}"#)
        }
    })
    .expect("bind the ephemeral llama-server stand-in");
    let ollama_port = spawn_fake_upstream(|head| {
        if head.starts_with("GET /api/tags") {
            raw_http_response(
                "application/json",
                br#"{"models":[{"name":"pinned-ollama-tag:latest"}]}"#,
            )
        } else {
            raw_http_response(UPSTREAM_EMBEDDINGS_CONTENT_TYPE, UPSTREAM_EMBEDDINGS_BODY)
        }
    })
    .expect("bind the ephemeral embeddings upstream stand-in");
    let endpoints = CliRuntimeEndpoints {
        llama_port,
        ollama_base_url: format!("http://127.0.0.1:{ollama_port}"),
    };
    let config = ApiServerConfig {
        expose_ollama: true,
        ..base_config()
    };
    let Some(server) = LegacyServer::start_with_endpoints("embeddings", config, endpoints).await
    else {
        return;
    };
    let client = http_client();
    let embeddings_body = r#"{"model":"pinned-ollama-tag:latest","input":"hello"}"#;

    // 1. Success: the upstream's bytes, unmodified, under a *fixed*
    //    `application/json` (not the upstream's own content-type), with the
    //    wildcard CORS origin.
    let response = client
        .post(server.url("/v1/embeddings"))
        .header("content-type", "application/json")
        .body(embeddings_body)
        .send()
        .await
        .expect("POST /v1/embeddings");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        cors_origin(&response).as_deref(),
        Some("*"),
        "a successful embeddings response must carry the wildcard CORS origin — m3's default is \
         deny-all, so a merge that adopts it silently breaks every browser client of this route"
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "embeddings stamps a fixed content-type rather than echoing the upstream's, unlike the \
         streaming chat path"
    );
    let expected_length = UPSTREAM_EMBEDDINGS_BODY.len().to_string();
    assert_eq!(
        response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some(expected_length.as_str()),
        "embeddings are never streamed — the body is buffered and keeps its exact length"
    );
    let body = response.bytes().await.expect("embeddings body");
    assert_bytes("POST /v1/embeddings", &body, UPSTREAM_EMBEDDINGS_BODY);
    // Spelled out separately from the byte compare, because each of these is a
    // quirk a re-serializing implementation would individually "fix" and a
    // named assertion says which one broke.
    assert!(
        body.starts_with(br#"{"object":"list","data":"#),
        "source key order must survive; alphabetical order means the body was re-serialized"
    );
    assert!(
        contains_bytes(&body, br#""x_upstream_note":"relayed verbatim""#),
        "an unknown upstream key must survive; a typed round-trip would drop it"
    );
    assert!(
        contains_bytes(&body, b"[1.0,-0.5]"),
        "the upstream's own float rendering must survive"
    );

    // 2. Failure, same route, same wildcard. The scope refusal is legacy's own
    //    `insufficient_scope` envelope, byte for byte — a different code *and*
    //    a different message from m3's `forbidden` / "token does not grant the
    //    requested scope", so a client branching on either sees which handler
    //    owns the route.
    let scoped_token = "lmk-embeddings-pin-token";
    server.set_config(ApiServerConfig {
        require_token: true,
        expose_ollama: true,
        tokens: vec![TokenEntry {
            id: "embeddings-pin".to_string(),
            label: "embeddings route pins".to_string(),
            sha256: token_digest(scoped_token),
            // Deliberately *not* `Embeddings`.
            scopes: vec![Scope::Models],
            backends: vec![Backend::Local, Backend::Ollama],
            ..TokenEntry::default()
        }],
        ..base_config()
    });
    let response = client
        .post(server.url("/v1/embeddings"))
        .bearer_auth(scoped_token)
        .header("content-type", "application/json")
        .body(embeddings_body)
        .send()
        .await
        .expect("POST /v1/embeddings with an under-scoped token");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        cors_origin(&response).as_deref(),
        Some("*"),
        "the header is not conditional on success — a browser needs it on the 403 most of all, or \
         the fetch rejects with an opaque network error instead of surfacing the status"
    );
    let denied = response.bytes().await.expect("embeddings 403 body");
    assert_bytes(
        "under-scoped embeddings token",
        &denied,
        error_envelope("insufficient_scope", EMBEDDINGS_SCOPE_ERROR_MESSAGE).as_bytes(),
    );

    // Structural belt-and-braces on the envelope, same as the 404 pin: no
    // fourth key inside `error`, nothing beside it at the top level. An
    // implementation that *added* `param` or `request_id` would still satisfy a
    // `contains` check and still break a client with a closed struct.
    let parsed: serde_json::Value =
        serde_json::from_slice(&denied).expect("the embeddings 403 body is JSON");
    let top = parsed.as_object().expect("a JSON object");
    assert_eq!(
        top.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["error"],
        "`error` is the only top-level key"
    );
    let inner = top["error"].as_object().expect("error is an object");
    assert_eq!(
        inner.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["code", "message", "type"],
        "exactly three keys, in this order"
    );

    // 3. And the pre-resolution failure, which is where the scope check's
    //    ordering shows: a missing `model` is legacy's `404 model_not_found`
    //    with OpenAI's own wording, still wildcard-CORS'd.
    server.set_config(ApiServerConfig {
        expose_ollama: true,
        ..base_config()
    });
    let response = client
        .post(server.url("/v1/embeddings"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST /v1/embeddings with no model");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(cors_origin(&response).as_deref(), Some("*"));
    assert_bytes(
        "embeddings with no model",
        &response.bytes().await.expect("embeddings 404 body"),
        error_envelope("model_not_found", "you must provide a model parameter").as_bytes(),
    );

    // Counter-test for the block: the same server still answers the success
    // case, so the failures above are route-specific refusals and not "this
    // fixture stopped working".
    let response = client
        .post(server.url("/v1/embeddings"))
        .header("content-type", "application/json")
        .body(embeddings_body)
        .send()
        .await
        .expect("POST /v1/embeddings again after the failure cases");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_bytes(
        "POST /v1/embeddings (repeat)",
        &response.bytes().await.expect("embeddings body"),
        UPSTREAM_EMBEDDINGS_BODY,
    );
}
