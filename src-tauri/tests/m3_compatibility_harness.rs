//! Phase 8 item 11 (OpenAI/Ollama API compatibility harness): real
//! integration tests that spin up the actual `m3_http_server` loopback
//! listener (through the explicitly test-only compatibility harness) in
//! front of a real `M3RuntimeHub`, and make real HTTP
//! requests against every advertised route with a real `reqwest::Client` —
//! following the `m3_http_server.rs`-embedded test's and
//! `m3_runtime_hub_contract.rs`'s convention of mocking only at the runtime
//! *driver* boundary (no real Ollama/llama.cpp process is available in
//! CI), never mocking the HTTP layer, translation layer, or auth/pairing
//! pipeline itself.
//!
//! Coverage: `/v1/models`, `/v1/chat/completions` (non-streaming and SSE
//! streaming), `/v1/responses`, `/v1/messages`, tool calls, JSON-schema
//! structured output, `/v1/embeddings` (both the genuine success path for
//! an embeddings-capable mock runtime and the honest `unsupported` failure
//! path for one that isn't — never a fabricated vector), the native-Ollama
//! `/api/tags` and `/api/chat`, and external bearer-token pairing/scope
//! enforcement (confirming the new routes are gated by the exact same
//! auth/pairing pipeline as the pre-existing ones).

use little_monkey_lib::compatibility_hub::{
    ApiBackend, ApiScope, LanServerPolicy, LanStateProtector, PairingRequest,
};
use little_monkey_lib::m3_http_server::{start_compatibility_harness, CompatibilityHarnessServer};
use little_monkey_lib::m3_production::{OpenAiCompatibleM3InferenceEngine, SystemM3HardwareProbe};
use little_monkey_lib::m3_runtime_hub::*;
use little_monkey_lib::runtime_adapter::{
    execution_support, AcceleratorKind, EndpointOrigin, EndpointPolicy, ExecutionSupport,
    HardwareSnapshot, HttpTransport, KeepAlive, ModelCapabilities, OllamaHttpAdapter,
    PlatformCapabilities, ReqwestHttpTransport, RuntimeAdapter, RuntimeDescriptor,
    RuntimeInventory, RuntimeKind, RuntimeLifecycleState, RuntimeLogTail, RuntimeModel,
    RuntimeStatus, SettingValue,
};
use little_monkey_lib::server::{
    run_cli_server_with_m3_hub_and_endpoints, save_config_impl, ApiServerConfig, Backend,
    CliRuntimeEndpoints, Scope, TokenEntry,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "m3-compat-harness-{label}-{}-{}",
            std::process::id(),
            next_test_id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn next_test_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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

struct TestProtector;

impl LanStateProtector for TestProtector {
    fn protector_id(&self) -> &str {
        "m3-compat-harness-test-protector"
    }

    fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
        let mut hash = Sha256::new();
        hash.update(b"m3-compat-harness-test-key");
        hash.update(canonical_state);
        Ok(hash.finalize().to_vec())
    }

    fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
        if self.authenticate(canonical_state)? == tag {
            Ok(())
        } else {
            Err("test state authentication failed".to_string())
        }
    }
}

/// A minimal, real `M3RuntimeDriver` — real in the sense that it is dispatched
/// through the actual HTTP server, hub, and translation layers exactly like
/// a production driver would be; only the "does this process actually talk
/// to a model" boundary is faked, matching `m3_runtime_hub_contract.rs`'s
/// `MockRuntimeDriver` convention.
struct MockRuntimeDriver {
    runtime_id: String,
    kind: M3RuntimeKind,
    backend: ApiBackend,
    model_id: String,
    supports_embed: bool,
    stream_control: Option<Arc<StreamControl>>,
}

#[derive(Default)]
struct StreamControl {
    started: Notify,
    release: Notify,
    cancellations: AtomicU64,
}

impl MockRuntimeDriver {
    fn ollama() -> Self {
        Self {
            runtime_id: "harness-ollama".to_string(),
            kind: M3RuntimeKind::Ollama,
            backend: ApiBackend::Ollama,
            model_id: "llama3".to_string(),
            supports_embed: true,
            stream_control: None,
        }
    }

    fn llama_cpp() -> Self {
        Self {
            runtime_id: "harness-llama".to_string(),
            kind: M3RuntimeKind::LlamaCpp,
            backend: ApiBackend::ManagedLocal,
            model_id: "qwen-local".to_string(),
            supports_embed: false,
            stream_control: None,
        }
    }

    fn controlled_llama_cpp(control: Arc<StreamControl>) -> Self {
        Self {
            runtime_id: "harness-llama".to_string(),
            kind: M3RuntimeKind::LlamaCpp,
            backend: ApiBackend::ManagedLocal,
            model_id: "qwen-local".to_string(),
            supports_embed: false,
            stream_control: Some(control),
        }
    }

    fn native_descriptor(&self) -> RuntimeDescriptor {
        RuntimeDescriptor {
            schema_version: little_monkey_lib::runtime_adapter::RUNTIME_ADAPTER_SCHEMA_VERSION,
            runtime_id: self.runtime_id.clone(),
            kind: match self.kind {
                M3RuntimeKind::Ollama => RuntimeKind::Ollama,
                _ => RuntimeKind::LlamaCpp,
            },
            label: self.runtime_id.clone(),
            endpoint: EndpointOrigin::parse("http://127.0.0.1:1", EndpointPolicy::LoopbackOnly)
                .expect("endpoint"),
            managed: true,
        }
    }
}

impl M3RuntimeDriver for MockRuntimeDriver {
    fn descriptor(&self) -> M3RuntimeDescriptor {
        M3RuntimeDescriptor {
            runtime_id: self.runtime_id.clone(),
            kind: self.kind,
            label: self.runtime_id.clone(),
            managed: true,
            api_backend: self.backend,
        }
    }

    fn capabilities(&self) -> M3RuntimeCapabilityView {
        M3RuntimeCapabilityView {
            descriptor: self.descriptor(),
            can_load: false,
            can_unload: false,
            can_logs: true,
            can_metrics: true,
            can_infer: true,
            can_embed: self.supports_embed,
            settings: Vec::new(),
        }
    }

    fn validate_config(&self, _values: &BTreeMap<String, SettingValue>) -> M3HubResult<()> {
        Ok(())
    }

    fn status<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeStatusView> {
        Box::pin(async move {
            Ok(M3RuntimeStatusView::Adapter {
                status: RuntimeStatus {
                    runtime: self.native_descriptor(),
                    state: RuntimeLifecycleState::Ready,
                    version: Some("harness-1".to_string()),
                    process: None,
                    message: None,
                    checked_at_ms: 20_000,
                },
                running_models: Vec::new(),
            })
        })
    }

    fn inventory<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            Ok(RuntimeInventory {
                schema_version: little_monkey_lib::runtime_adapter::RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id: self.runtime_id.clone(),
                models: vec![RuntimeModel {
                    model_id: self.model_id.clone(),
                    display_name: self.model_id.clone(),
                    size_bytes: 4 * 1024 * 1024 * 1024,
                    local_path: None,
                    digest: Some("deadbeef".to_string()),
                    modified_at: None,
                    capabilities: ModelCapabilities {
                        chat: true,
                        embeddings: self.supports_embed,
                        tool_calling: true,
                        vision: false,
                    },
                    metadata: BTreeMap::new(),
                }],
                captured_at_ms: 20_000,
            })
        })
    }

    fn load<'a>(
        &'a self,
        _model: &'a M3ResolvedModel,
        _settings: &'a BTreeMap<String, SettingValue>,
        _keep_alive: Option<KeepAlive>,
        _replace_existing: bool,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn unload<'a>(
        &'a self,
        _model_id: &'a str,
        _force_exact_owner: bool,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn logs<'a>(
        &'a self,
        _max_bytes: usize,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            Ok(RuntimeLogTail {
                text: String::new(),
                truncated: false,
            })
        })
    }

    fn metrics<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeMetricsView> {
        Box::pin(async move {
            Ok(M3RuntimeMetricsView::Adapter {
                status: RuntimeStatus {
                    runtime: self.native_descriptor(),
                    state: RuntimeLifecycleState::Ready,
                    version: Some("harness-1".to_string()),
                    process: None,
                    message: None,
                    checked_at_ms: 20_000,
                },
                running_models: Vec::new(),
            })
        })
    }

    fn complete<'a>(
        &'a self,
        request: &'a little_monkey_lib::compatibility_hub::CanonicalInferenceRequest,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, little_monkey_lib::compatibility_hub::CanonicalInferenceResponse> {
        Box::pin(async move {
            use little_monkey_lib::compatibility_hub::{
                CanonicalContent, CanonicalInferenceResponse, CanonicalUsage,
            };
            let mut content = vec![CanonicalContent::Text {
                text: "functional response".to_string(),
            }];
            let mut finish_reason = "stop".to_string();
            if let Some(tool) = request.tools.first() {
                content.push(CanonicalContent::ToolUse {
                    id: format!("call-{}", request.request_id),
                    name: tool.name.clone(),
                    input: json!({"echo": true}),
                });
                finish_reason = "tool_calls".to_string();
            }
            Ok(CanonicalInferenceResponse {
                response_id: format!("response-{}", request.request_id),
                model: request.model.clone(),
                content,
                finish_reason,
                usage: CanonicalUsage {
                    input_tokens: 8,
                    output_tokens: 3,
                    cached_input_tokens: None,
                },
                created_at_seconds: 1_700_000_000,
            })
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a little_monkey_lib::compatibility_hub::CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            if let Some(control) = &self.stream_control {
                control.started.notify_one();
                control.release.notified().await;
            }
            use little_monkey_lib::compatibility_hub::{CanonicalStreamEvent, CanonicalUsage};
            let response_id = format!("response-{}", request.request_id);
            sink.emit(CanonicalStreamEvent::ResponseStart {
                response_id: response_id.clone(),
                model: request.model.clone(),
                created_at_seconds: 1_700_000_000,
            })
            .map_err(M3HubError::Runtime)?;
            sink.emit(CanonicalStreamEvent::TextStart { index: 0 })
                .map_err(M3HubError::Runtime)?;
            sink.emit(CanonicalStreamEvent::TextDelta {
                index: 0,
                text: "streamed".to_string(),
            })
            .map_err(M3HubError::Runtime)?;
            sink.emit(CanonicalStreamEvent::TextEnd { index: 0 })
                .map_err(M3HubError::Runtime)?;
            sink.emit(CanonicalStreamEvent::ResponseCompleted {
                response_id,
                finish_reason: "stop".to_string(),
                usage: CanonicalUsage {
                    input_tokens: 8,
                    output_tokens: 1,
                    cached_input_tokens: None,
                },
            })
            .map_err(M3HubError::Runtime)
        })
    }

    fn cancel<'a>(
        &'a self,
        _request_id: &'a str,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        Box::pin(async move {
            let Some(control) = &self.stream_control else {
                return Ok(false);
            };
            control.cancellations.fetch_add(1, Ordering::SeqCst);
            control.release.notify_one();
            Ok(true)
        })
    }

    fn embed<'a>(
        &'a self,
        request: &'a little_monkey_lib::compatibility_hub::CanonicalEmbeddingRequest,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, little_monkey_lib::compatibility_hub::CanonicalEmbeddingResponse> {
        Box::pin(async move {
            if !self.supports_embed {
                // Falls through to the trait default (honest `Unsupported`)
                // by constructing the same error it would produce — this
                // mock exists specifically to prove the *dispatch* path
                // routes here rather than fabricating a vector, so it
                // asserts the same contract explicitly.
                return Err(M3HubError::Unsupported(format!(
                    "runtime {} does not support embeddings generation for model {}",
                    self.runtime_id, request.model
                )));
            }
            use little_monkey_lib::compatibility_hub::{
                CanonicalEmbeddingDatum, CanonicalEmbeddingResponse, CanonicalUsage,
            };
            let data = request
                .input
                .iter()
                .enumerate()
                .map(|(index, text)| CanonicalEmbeddingDatum {
                    index,
                    // Deterministic, real (not fabricated-as-fake-success)
                    // stand-in vector derived from the actual input length.
                    embedding: vec![text.len() as f32, index as f32],
                })
                .collect::<Vec<_>>();
            Ok(CanonicalEmbeddingResponse {
                model: request.model.clone(),
                data,
                usage: CanonicalUsage {
                    input_tokens: request.input.iter().map(|text| text.len() as u64).sum(),
                    output_tokens: 0,
                    cached_input_tokens: None,
                },
            })
        })
    }
}

fn test_hub(root: &TestDirectory) -> Arc<M3RuntimeHub> {
    test_hub_with_runtimes(
        root,
        vec![
            Arc::new(MockRuntimeDriver::ollama()),
            Arc::new(MockRuntimeDriver::llama_cpp()),
        ],
    )
}

fn test_hub_with_runtimes(
    root: &TestDirectory,
    runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
) -> Arc<M3RuntimeHub> {
    hub_with(root, runtimes, Arc::new(TestHardware))
}

/// The hub every test in this file runs behind, with the hardware probe left
/// open: every route here uses the synthetic [`TestHardware`], and the one
/// opt-in accelerator route below uses the real `SystemM3HardwareProbe`
/// because its whole point is that the machine, not a fixture, answered.
fn hub_with(
    root: &TestDirectory,
    runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
    hardware: Arc<dyn M3HardwareProbe>,
) -> Arc<M3RuntimeHub> {
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
                hardware,
                download,
                catalogs: Vec::new(),
                runtimes,
                runtime_reconciler: None,
                lan_factory: Some(Arc::new(DefaultM3LanAccessFactory::new(
                    Arc::new(little_monkey_lib::compatibility_hub::OsLanEntropy),
                    Arc::new(TestProtector),
                ))),
            },
        )
        .expect("compatibility harness test hub"),
    )
}

/// Every port this process has already handed out.
///
/// The pick below releases its listener so the server under test can rebind
/// the port, and until that rebind lands the port is genuinely free — which
/// means a pick running in a parallel test can be handed the very same port,
/// and one of the two servers then loses the bind. Refusing to issue a port
/// twice closes that window inside this binary; the retry in
/// [`start_test_server`] covers the rest.
static HANDED_OUT_PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

/// How many times [`start_test_server`] re-picks a port when the listener
/// would not bind. Three is enough for a lost port race and few enough that a
/// harness that cannot start still fails fast.
const START_ATTEMPTS: usize = 3;

/// Bind, keep the port, drop the listener so the server under test can rebind
/// it, and treat a sandbox that forbids listeners as "skip", not "fail".
///
/// Never answers with a port this binary has already used: the rejected
/// listeners are held until a fresh one comes back, so the OS cannot keep
/// offering the same port.
async fn free_loopback_port() -> Option<u16> {
    let mut rejected = Vec::new();
    for _ in 0..16 {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping compatibility harness: sandbox forbids local listeners");
                return None;
            }
            Err(error) => panic!("bind ephemeral test port: {error}"),
        };
        let port = listener.local_addr().expect("ephemeral address").port();
        if HANDED_OUT_PORTS
            .lock()
            .expect("handed-out port registry")
            .insert(port)
        {
            return Some(port);
        }
        rejected.push(listener);
    }
    panic!("the OS kept handing back ports this test binary has already used");
}

/// Starts the real M3 HTTP server (loopback, unauthenticated — a
/// pre-existing, deliberate policy for `127.0.0.1` when
/// `require_authentication` is false) in front of `hub` and returns
/// `(state, base_url)`. The caller must stop the harness when done.
///
/// Re-picks the port when the bind loses it to another process between the
/// pick and the bind. The last attempt still panics with the real error, so a
/// harness that genuinely cannot start is reported rather than retried into
/// silence.
async fn start_test_server(hub: Arc<M3RuntimeHub>) -> Option<(CompatibilityHarnessServer, String)> {
    for attempt in 1..=START_ATTEMPTS {
        let port = free_loopback_port().await?;
        let mut policy = LanServerPolicy::default();
        policy.port = port;
        policy.require_authentication = false;
        hub.configure_lan(policy)
            .expect("configure loopback policy");
        match start_compatibility_harness(hub.clone()).await {
            Ok((state, started)) => {
                assert_eq!(started.status, "running");
                return Some((state, format!("http://127.0.0.1:{port}")));
            }
            Err(error) if attempt < START_ATTEMPTS => eprintln!(
                "harness start attempt {attempt} on port {port} failed ({error}); \
                 retrying on a fresh port"
            ),
            Err(error) => panic!("start M3 HTTP server: {error}"),
        }
    }
    unreachable!("the last attempt either returns or panics");
}

/// Waits for a `run_cli_server_*` task to answer `/health`, and says why when
/// it never will.
///
/// That server binds inside the spawned task, so a port lost between the pick
/// and the bind surfaces here — as a silent ten-second timeout unless the
/// task's own error is read, which is what this reads. Same budget as the
/// `tokio::time::timeout` it replaces: 500 tries, 20ms apart.
async fn await_cli_server_readiness(
    server: &mut tokio::task::JoinHandle<Result<(), String>>,
    base: &str,
    client: &reqwest::Client,
) {
    for _ in 0..500 {
        if client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        if server.is_finished() {
            let outcome = server.await;
            panic!("the server exited before readiness: {outcome:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the server never answered /health on {base}");
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("loopback client")
}

/// Wall-clock milliseconds, matching the clock the HTTP layer authorizes
/// against (`m3_http_server`'s credential preflight and quota debit both use
/// real time, not a hub-injected clock).
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `now_ms` only orders the pairing challenge against its own completion, so a
/// small synthetic value is fine there. **Token expiry is not**: it is checked
/// against the wall clock, so `now_ms + 100_000` on a synthetic `now_ms` mints a
/// token that expired in 1970 and every request with it answers `403 token is
/// expired`.
fn pair_harness_token(hub: &M3RuntimeHub, label: &str, now_ms: u64) -> String {
    let challenge = hub
        .begin_pairing(
            PairingRequest {
                client_label: label.to_string(),
                scopes: BTreeSet::from([ApiScope::ChatCompletions]),
                backends: BTreeSet::from([ApiBackend::ManagedLocal]),
                allowed_models: BTreeSet::from(["qwen-local".to_string()]),
                token_expires_at_ms: Some(wall_clock_ms() + 600_000),
            },
            now_ms,
            "127.0.0.1",
        )
        .expect("begin harness token pairing");
    hub.complete_pairing(
        &challenge.challenge_id,
        &challenge.pairing_code,
        now_ms + 1,
        "127.0.0.1",
    )
    .expect("complete harness token pairing")
    .token
}

/// The registry is the whole reason two harnesses in this binary cannot be
/// handed the same port during the window between a pick and its rebind, so it
/// is worth one direct assertion rather than only the tests that depend on it.
#[tokio::test]
async fn a_loopback_port_is_never_handed_out_twice() {
    let mut seen = BTreeSet::new();
    for _ in 0..64 {
        let Some(port) = free_loopback_port().await else {
            return;
        };
        assert!(seen.insert(port), "port {port} was handed out twice");
    }
}

#[tokio::test]
async fn streaming_is_registered_before_http_200_and_cancel_is_owner_bound() {
    let root = TestDirectory::new("stream-registration-cancel");
    let control = Arc::new(StreamControl::default());
    let hub = test_hub_with_runtimes(
        &root,
        vec![Arc::new(MockRuntimeDriver::controlled_llama_cpp(
            control.clone(),
        ))],
    );
    let Some(port) = free_loopback_port().await else {
        return;
    };
    let mut policy = LanServerPolicy::default();
    policy.port = port;
    policy.require_authentication = true;
    policy.rate_limit.max_requests = 100;
    policy.rate_limit.max_input_bytes = 16 * 1024 * 1024;
    hub.configure_lan(policy)
        .expect("configure authenticated stream harness");
    let owner_token = pair_harness_token(&hub, "stream-owner", 40_000);
    let other_token = pair_harness_token(&hub, "same-capability-other-owner", 40_010);
    let (state, started) = start_compatibility_harness(hub)
        .await
        .expect("start authenticated stream harness");
    assert_eq!(started.status, "running");
    let base = format!("http://127.0.0.1:{port}");
    let client = http_client();
    let stream_body = json!({
        "model":"qwen-local",
        "messages":[{"role":"user","content":"hold stream"}],
        "stream":true
    });
    let cancel_body = |request_id: &str| {
        json!({
            "protocol":"open_ai_chat_completions",
            "runtimeId":"harness-llama",
            "requestId":request_id,
            "modelId":"qwen-local"
        })
    };

    // Generous, because this asserts a *shape* and not a latency: the mock holds
    // the stream open until `control.release` fires far below, so a response that
    // waited for the body would never arrive at all. Two seconds was tight enough
    // that a loaded Windows runner tripped it on a cold first connection.
    let first = tokio::time::timeout(
        Duration::from_secs(30),
        client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&owner_token)
            .header("x-request-id", "http-owned-stream")
            .json(&stream_body)
            .send(),
    )
    .await
    .expect("stream headers wait only for registration")
    .expect("owned stream request");
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert_eq!(
        first
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let foreign = client
        .post(format!("{base}/v1/requests/cancel"))
        .bearer_auth(&other_token)
        .json(&cancel_body("http-owned-stream"))
        .send()
        .await
        .expect("foreign owner cancel request");
    let foreign_status = foreign.status();
    let foreign_body = foreign.text().await.expect("foreign cancel body");
    let missing = client
        .post(format!("{base}/v1/requests/cancel"))
        .bearer_auth(&other_token)
        .json(&cancel_body("http-missing-stream"))
        .send()
        .await
        .expect("missing cancel request");
    let missing_status = missing.status();
    let missing_body = missing.text().await.expect("missing cancel body");
    assert_eq!(foreign_status, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(missing_status, foreign_status);
    assert_eq!(missing_body, foreign_body);
    assert_eq!(control.cancellations.load(Ordering::SeqCst), 0);

    let exact = client
        .post(format!("{base}/v1/requests/cancel"))
        .bearer_auth(&owner_token)
        .json(&cancel_body("http-owned-stream"))
        .send()
        .await
        .expect("exact owner cancel request");
    assert_eq!(exact.status(), reqwest::StatusCode::OK);
    assert_eq!(control.cancellations.load(Ordering::SeqCst), 1);
    let first_body = first.text().await.expect("cancelled stream body");
    assert!(first_body.contains("[DONE]"));

    let active_duplicate = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&owner_token)
        .header("x-request-id", "http-duplicate-stream")
        .json(&stream_body)
        .send()
        .await
        .expect("first duplicate-id stream request");
    assert_eq!(active_duplicate.status(), reqwest::StatusCode::OK);
    let duplicate = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&owner_token)
        .header("x-request-id", "http-duplicate-stream")
        .json(&stream_body)
        .send()
        .await
        .expect("second duplicate-id stream request");
    assert_eq!(duplicate.status(), reqwest::StatusCode::CONFLICT);
    assert_ne!(
        duplicate
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let release_duplicate = client
        .post(format!("{base}/v1/requests/cancel"))
        .bearer_auth(&owner_token)
        .json(&cancel_body("http-duplicate-stream"))
        .send()
        .await
        .expect("release duplicate-id owner stream");
    assert_eq!(release_duplicate.status(), reqwest::StatusCode::OK);
    let _ = active_duplicate
        .text()
        .await
        .expect("released duplicate stream body");
    assert_eq!(control.cancellations.load(Ordering::SeqCst), 2);
    state.stop().await;
}

#[tokio::test]
async fn v1_models_and_api_tags_list_the_scoped_runtimes_models() {
    let root = TestDirectory::new("models-tags");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

    let union = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("union v1/models request");
    assert_eq!(union.status(), reqwest::StatusCode::OK);
    let union_payload: Value = union.json().await.expect("union v1/models JSON");
    let union_ids = union_payload["data"]
        .as_array()
        .expect("union data array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("union model id"))
        .collect::<Vec<_>>();
    assert!(
        union_ids.contains(&"llama3"),
        "expected Ollama model in {union_ids:?}"
    );
    assert!(
        union_ids.contains(&"qwen-local"),
        "expected managed model in {union_ids:?}"
    );

    let models = client
        .get(format!("{base}/v1/models"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .send()
        .await
        .expect("v1/models request");
    assert_eq!(models.status(), reqwest::StatusCode::OK);
    let payload: Value = models.json().await.expect("v1/models JSON");
    assert_eq!(payload["object"], "list");
    let ids: Vec<&str> = payload["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&"llama3"), "expected llama3 in {ids:?}");

    let tags = client
        .get(format!("{base}/api/tags"))
        .header("x-little-monkey-runtime-id", "harness-llama")
        .send()
        .await
        .expect("api/tags request");
    assert_eq!(tags.status(), reqwest::StatusCode::OK);
    let payload: Value = tags.json().await.expect("api/tags JSON");
    let models = payload["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["name"], "qwen-local");
    assert_eq!(models[0]["model"], "qwen-local");
    assert!(models[0]["modified_at"].as_str().unwrap().contains('T'));

    let unknown = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"not-advertised","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("unknown exact model request");
    let unknown_status = unknown.status();
    let unknown_body = unknown.text().await.expect("unknown model body");
    assert_eq!(
        unknown_status,
        reqwest::StatusCode::NOT_FOUND,
        "unexpected unknown-model response: {unknown_body}"
    );
    assert!(unknown_body.contains("not-advertised"));

    state.stop().await;
}

#[tokio::test]
async fn chat_completions_non_streaming_streaming_tool_calls_and_json_schema() {
    let root = TestDirectory::new("chat");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

    // Non-streaming, plain text.
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"hello"}],
            "stream": false
        }))
        .send()
        .await
        .expect("chat completions request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let payload: Value = response.json().await.expect("chat completions JSON");
    assert_eq!(payload["object"], "chat.completion");
    assert_eq!(
        payload["choices"][0]["message"]["content"],
        "functional response"
    );

    // Tool calls: request carries a tool, mock echoes a tool_calls choice.
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"weather?"}],
            "tools": [{"type":"function","function":{"name":"weather","description":"Weather","parameters":{"type":"object"}}}],
            "stream": false
        }))
        .send()
        .await
        .expect("tool call request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let payload: Value = response.json().await.expect("tool call JSON");
    assert_eq!(payload["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        payload["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "weather"
    );

    // JSON-schema structured output: request must be accepted (not
    // rejected as unsupported) once a schema is present.
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"give me json"}],
            "response_format": {"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"}}},
            "stream": false
        }))
        .send()
        .await
        .expect("json schema request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Streaming: real SSE bytes over the wire, parsed as text/event-stream.
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("streaming chat request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.text().await.expect("SSE body");
    assert!(body.contains("chat.completion.chunk"));
    assert!(body.contains("\"content\":\"streamed\""));
    assert!(body.contains("[DONE]"));

    state.stop().await;
}

#[tokio::test]
async fn responses_and_messages_protocols_translate_end_to_end() {
    let root = TestDirectory::new("responses-messages");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

    let response = client
        .post(format!("{base}/v1/responses"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "input": "hello",
            "stream": false
        }))
        .send()
        .await
        .expect("responses request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let payload: Value = response.json().await.expect("responses JSON");
    assert_eq!(payload["object"], "response");
    assert_eq!(payload["status"], "completed");

    let response = client
        .post(format!("{base}/v1/messages"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "max_tokens": 64,
            "messages": [{"role":"user","content":"hello"}],
            "stream": false
        }))
        .send()
        .await
        .expect("messages request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let payload: Value = response.json().await.expect("messages JSON");
    assert_eq!(payload["type"], "message");
    assert_eq!(payload["content"][0]["text"], "functional response");

    state.stop().await;
}

#[tokio::test]
async fn embeddings_succeeds_for_a_real_embeddings_capable_runtime_and_is_honestly_unsupported_otherwise(
) {
    let root = TestDirectory::new("embeddings");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

    // Genuine success path: the mock Ollama-kind runtime really produces
    // vectors (not fabricated post-hoc — they come back through the same
    // dispatch_embeddings -> driver.embed() path production code uses).
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({"model": "llama3", "input": ["hello", "world"]}))
        .send()
        .await
        .expect("embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let payload: Value = response.json().await.expect("embeddings JSON");
    assert_eq!(payload["object"], "list");
    let data = payload["data"].as_array().expect("data array");
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["embedding"][0], 5.0); // len("hello") == 5
    assert_eq!(data[1]["embedding"][0], 5.0); // len("world") == 5
    assert_eq!(payload["usage"]["prompt_tokens"], 10);

    // Honest failure path: the mock llama.cpp-kind runtime has no embed
    // support (matching the real managed llama.cpp chat instance, started
    // without `--embeddings`). Must fail clearly, never with a 200 and a
    // fabricated vector.
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-llama")
        .json(&json!({"model": "qwen-local", "input": "hello"}))
        .send()
        .await
        .expect("unsupported embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let payload: Value = response.json().await.expect("unsupported embeddings JSON");
    assert_eq!(payload["error"]["code"], "unsupported");

    // Rejects the base64 encoding format and dimensions truncation
    // honestly rather than silently mis-encoding or truncating.
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({"model": "llama3", "input": "hello", "encoding_format": "base64"}))
        .send()
        .await
        .expect("base64 embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    state.stop().await;
}

#[tokio::test]
async fn ollama_native_chat_returns_ollama_shaped_response_with_tool_calls() {
    let root = TestDirectory::new("ollama-chat");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

    let response = client
        .post(format!("{base}/api/chat"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"weather?"}],
            "tools": [{"type":"function","function":{"name":"weather","description":"Weather","parameters":{"type":"object"}}}],
            "stream": false
        }))
        .send()
        .await
        .expect("ollama chat request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let payload: Value = response.json().await.expect("ollama chat JSON");
    assert_eq!(payload["model"], "llama3");
    assert_eq!(payload["done"], true);
    assert_eq!(payload["message"]["role"], "assistant");
    assert_eq!(
        payload["message"]["tool_calls"][0]["function"]["name"],
        "weather"
    );
    assert!(payload["total_duration"].as_u64().is_some());
    assert_eq!(payload["prompt_eval_count"], 8);

    // A `stream:true` (or omitted) request gets NDJSON framing — still the
    // complete response as one line (documented streaming limitation), not
    // a rejection.
    let response = client
        .post(format!("{base}/api/chat"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"hello"}]
        }))
        .send()
        .await
        .expect("streaming-flagged ollama chat request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/x-ndjson")
    );
    let body = response.text().await.expect("ndjson body");
    assert_eq!(body.lines().count(), 1, "expected exactly one NDJSON line");
    let parsed: Value = serde_json::from_str(body.trim()).expect("ndjson line is valid JSON");
    assert_eq!(parsed["done"], true);

    // Vision content is honestly rejected, not silently dropped.
    let response = client
        .post(format!("{base}/api/chat"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({
            "model": "llama3",
            "messages": [{"role":"user","content":"describe","images":["ZmFrZQ=="]}]
        }))
        .send()
        .await
        .expect("image ollama chat request");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    state.stop().await;
}

#[tokio::test]
async fn external_callers_are_gated_by_the_same_pairing_scopes_as_pre_existing_routes() {
    let root = TestDirectory::new("auth");
    let hub = test_hub(&root);
    let Some(port) = free_loopback_port().await else {
        return;
    };
    let mut policy = LanServerPolicy::default();
    policy.port = port;
    policy.require_authentication = true;
    hub.configure_lan(policy)
        .expect("configure authenticated policy");

    let chat_only = hub
        .begin_pairing(
            PairingRequest {
                client_label: "chat-only client".to_string(),
                scopes: BTreeSet::from([ApiScope::ChatCompletions]),
                backends: BTreeSet::from([ApiBackend::Ollama, ApiBackend::ManagedLocal]),
                allowed_models: BTreeSet::new(),
                token_expires_at_ms: None,
            },
            1_000,
            "127.0.0.1",
        )
        .expect("begin pairing");
    let chat_only_token = hub
        .complete_pairing(
            &chat_only.challenge_id,
            &chat_only.pairing_code,
            1_000,
            "127.0.0.1",
        )
        .expect("complete pairing");

    let embeddings_scoped = hub
        .begin_pairing(
            PairingRequest {
                client_label: "embeddings client".to_string(),
                scopes: BTreeSet::from([ApiScope::Embeddings]),
                backends: BTreeSet::from([ApiBackend::Ollama, ApiBackend::ManagedLocal]),
                allowed_models: BTreeSet::new(),
                token_expires_at_ms: None,
            },
            1_000,
            "127.0.0.1",
        )
        .expect("begin pairing");
    let embeddings_token = hub
        .complete_pairing(
            &embeddings_scoped.challenge_id,
            &embeddings_scoped.pairing_code,
            1_000,
            "127.0.0.1",
        )
        .expect("complete pairing");

    let (state, started) = start_compatibility_harness(hub)
        .await
        .expect("start M3 HTTP server");
    assert_eq!(started.status, "running");
    let base = format!("http://127.0.0.1:{port}");
    let client = http_client();

    // No bearer token at all: rejected exactly like the pre-existing routes.
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .json(&json!({"model": "llama3", "input": "hello"}))
        .send()
        .await
        .expect("unauthenticated embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    // A token paired only for ChatCompletions must not reach embeddings.
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .bearer_auth(&chat_only_token.token)
        .json(&json!({"model": "llama3", "input": "hello"}))
        .send()
        .await
        .expect("under-scoped embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    // The ChatCompletions-scoped token *does* work for the Ollama-native
    // `/api/chat` endpoint, since it reuses that scope by design (same
    // operation, different wire format — not a new, less-guarded class).
    let response = client
        .post(format!("{base}/api/chat"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .bearer_auth(&chat_only_token.token)
        .json(&json!({"model": "llama3", "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("scoped ollama chat request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // A correctly embeddings-scoped token succeeds.
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-little-monkey-runtime-id", "harness-ollama")
        .bearer_auth(&embeddings_token.token)
        .json(&json!({"model": "llama3", "input": "hello"}))
        .send()
        .await
        .expect("scoped embeddings request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    state.stop().await;
}

#[tokio::test]
async fn legacy_routes_dispatch_resolved_m3_targets_directly_for_chat_and_embeddings() {
    let root = TestDirectory::new("legacy-direct-m3");
    let hub = test_hub(&root);
    let Some(port) = free_loopback_port().await else {
        return;
    };
    let plaintext = "lmk-legacy-direct-m3";
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    let mut config = ApiServerConfig::default();
    config.port = port;
    config.require_token = true;
    config.expose_ollama = true;
    config.tokens = vec![TokenEntry {
        id: "legacy-direct-m3".to_string(),
        label: "legacy direct M3".to_string(),
        sha256: format!("{:x}", hasher.finalize()),
        scopes: vec![Scope::Chat, Scope::Embeddings],
        backends: vec![Backend::Local, Backend::Ollama],
        created_at: 1,
        last_used_at: None,
        expires_at: None,
        bound_local_app_id: None,
    }];
    let config_path = root.0.join("api_server.json");
    save_config_impl(&config_path, &config).expect("save legacy direct-M3 config");
    let mut server = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
        port,
        config_path,
        hub,
        CliRuntimeEndpoints {
            llama_port: 1,
            ollama_base_url: "http://127.0.0.1:1".to_string(),
        },
        Vec::new,
    ));
    let client = http_client();
    let base = format!("http://127.0.0.1:{port}");
    await_cli_server_readiness(&mut server, &base, &client).await;

    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(plaintext)
        .json(&json!({
            "model": "qwen-local",
            "messages": [{"role":"user","content":"hello"}],
            "stream": false
        }))
        .send()
        .await
        .expect("legacy-to-M3 chat request");
    let chat_status = chat.status();
    let chat_body: Value = chat.json().await.expect("legacy-to-M3 chat JSON");
    assert_eq!(chat_status, reqwest::StatusCode::OK, "{chat_body}");
    assert_eq!(
        chat_body["choices"][0]["message"]["content"],
        "functional response"
    );

    let embeddings = client
        .post(format!("{base}/v1/embeddings"))
        .bearer_auth(plaintext)
        .json(&json!({"model":"llama3","input":"hello"}))
        .send()
        .await
        .expect("legacy-to-M3 embeddings request");
    let embeddings_status = embeddings.status();
    let embeddings_body: Value = embeddings
        .json()
        .await
        .expect("legacy-to-M3 embeddings JSON");
    assert_eq!(
        embeddings_status,
        reqwest::StatusCode::OK,
        "{embeddings_body}"
    );
    assert_eq!(embeddings_body["data"][0]["embedding"], json!([5.0, 0.0]));

    server.abort();
    let _ = server.await;
}

/// The crossover that "one HTTP server" actually means, on a live socket.
///
/// Each token family was already covered in isolation: the legacy digest-list
/// path by `tests/legacy_route_compatibility.rs`, the pairing-store path by the
/// tests above. What was untested was the *dual accept* itself — one primary
/// loopback endpoint, one server generation, both families presented to it, each
/// routed to its own owner with its own bytes and its own quota ledger. The only
/// other test that sends a `lmk-lan-` token to the primary socket
/// (`server.rs`'s `saturated_dedup_socket_uses_the_selected_routes_own_busy_envelope`)
/// never checks that string against the pairing store at all; it exercises
/// classification plus a 503 envelope.
///
/// The prefix ordering is what makes the crossover subtle: `lmk-lan-` *starts
/// with* `lmk-`, so a classifier that tested the legacy prefix first would hand
/// every paired token to the legacy digest list, where it would 401 as an unknown
/// key. `classify_bearer_family` checks `lmk-lan-` first for exactly this reason,
/// and this test is what proves the ordering holds through a real socket rather
/// than only in the unit test next to it.
#[tokio::test]
async fn one_primary_socket_accepts_both_token_families_and_routes_each_to_its_owner() {
    let root = TestDirectory::new("dual-accept");
    let hub = test_hub(&root);
    let Some(port) = free_loopback_port().await else {
        return;
    };

    // `max_requests: 1` is the whole point of the rate-limit half: it exhausts
    // the *pairing* ledger after one request while leaving the legacy limiter
    // (a separate, in-memory, 60-request budget in `http_policy.rs`) untouched.
    let mut policy = LanServerPolicy::default();
    policy.port = port;
    policy.require_authentication = true;
    policy.rate_limit.max_requests = 1;
    hub.configure_lan(policy)
        .expect("configure the primary socket's pairing policy");

    let pairing_scopes = BTreeSet::from([ApiScope::ModelDiscover, ApiScope::ChatCompletions]);
    let pairing_backends = BTreeSet::from([ApiBackend::ManagedLocal, ApiBackend::Ollama]);
    let paired = hub
        .begin_pairing(
            PairingRequest {
                client_label: "dual-accept paired client".to_string(),
                scopes: pairing_scopes.clone(),
                backends: pairing_backends.clone(),
                allowed_models: BTreeSet::new(),
                token_expires_at_ms: None,
            },
            wall_clock_ms(),
            "127.0.0.1",
        )
        .expect("begin pairing");
    let paired_token = hub
        .complete_pairing(
            &paired.challenge_id,
            &paired.pairing_code,
            wall_clock_ms(),
            "127.0.0.1",
        )
        .expect("complete pairing")
        .token;
    assert!(
        paired_token.starts_with("lmk-lan-") && paired_token.len() == "lmk-lan-".len() + 64,
        "a real pairing token is `lmk-lan-` + 64 hex: {paired_token}"
    );

    // Expired at request time, but still in the future when it was minted, so
    // pairing itself accepts it.
    let expiring = hub
        .begin_pairing(
            PairingRequest {
                client_label: "dual-accept expired client".to_string(),
                scopes: pairing_scopes,
                backends: pairing_backends,
                allowed_models: BTreeSet::new(),
                token_expires_at_ms: Some(wall_clock_ms() - 1_000),
            },
            wall_clock_ms() - 5_000,
            "127.0.0.1",
        )
        .expect("begin pairing for an already-elapsed expiry");
    let expired_paired_token = hub
        .complete_pairing(
            &expiring.challenge_id,
            &expiring.pairing_code,
            wall_clock_ms() - 4_000,
            "127.0.0.1",
        )
        .expect("complete pairing for an already-elapsed expiry")
        .token;

    // Real `lmk-` + 32 hex plaintexts, stored the way the desktop stores them:
    // only the digest survives in `api_server.json`.
    let legacy_token = "lmk-0123456789abcdef0123456789abcdef";
    let expired_legacy_token = "lmk-fedcba9876543210fedcba9876543210";
    let unknown_legacy_token = "lmk-99999999999999999999999999999999";
    let unknown_paired_token = format!("lmk-lan-{}", "a".repeat(64));
    let legacy_entry = |id: &str, plaintext: &str, expires_at: Option<u64>| {
        let mut hasher = Sha256::new();
        hasher.update(plaintext.as_bytes());
        TokenEntry {
            id: id.to_string(),
            label: id.to_string(),
            sha256: format!("{:x}", hasher.finalize()),
            scopes: vec![Scope::Models, Scope::Chat, Scope::Embeddings],
            backends: vec![Backend::Local, Backend::Ollama],
            created_at: 1,
            last_used_at: None,
            expires_at,
            bound_local_app_id: None,
        }
    };

    let mut config = ApiServerConfig::default();
    config.port = port;
    config.require_token = true;
    // Left off deliberately: with an unreachable Ollama endpoint an enabled
    // Ollama source would turn `GET /v1/models` into an upstream probe, and this
    // test is about which owner answers, not about upstream reachability.
    config.expose_ollama = false;
    config.tokens = vec![
        legacy_entry("dual-accept-legacy", legacy_token, None),
        legacy_entry("dual-accept-legacy-expired", expired_legacy_token, Some(1)),
    ];
    let config_path = root.0.join("api_server.json");
    save_config_impl(&config_path, &config).expect("save dual-accept config");

    let mut server = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
        port,
        config_path,
        hub,
        CliRuntimeEndpoints {
            llama_port: 1,
            ollama_base_url: "http://127.0.0.1:1".to_string(),
        },
        Vec::new,
    ));
    let client = http_client();
    let base = format!("http://127.0.0.1:{port}");
    await_cli_server_readiness(&mut server, &base, &client).await;

    let models = |token: &str| {
        client
            .get(format!("{base}/v1/models"))
            .bearer_auth(token)
            .send()
    };

    // 1. The legacy family reaches the legacy owner: wildcard CORS, and rows
    //    without M3's extended `source_id`/`runtime_id`/`backend` fields.
    let legacy = models(legacy_token).await.expect("legacy models request");
    assert_eq!(legacy.status(), reqwest::StatusCode::OK);
    assert_eq!(
        legacy
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "the legacy owner stamps wildcard CORS on every response"
    );
    let legacy_body: Value = legacy.json().await.expect("legacy models JSON");
    let legacy_rows = legacy_body["data"]
        .as_array()
        .expect("legacy models data array");
    assert!(!legacy_rows.is_empty(), "{legacy_body}");
    for row in legacy_rows {
        assert!(
            row.get("source_id").is_none(),
            "the legacy listing is the unextended shape: {row}"
        );
    }

    // 2. The pairing family reaches the M3 owner on the *same socket in the same
    //    generation*: extended rows, and no wildcard CORS.
    let paired_response = models(&paired_token).await.expect("paired models request");
    assert_eq!(paired_response.status(), reqwest::StatusCode::OK);
    assert_ne!(
        paired_response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "the M3 owner is deny-by-default on CORS and must never emit the wildcard"
    );
    let paired_body: Value = paired_response.json().await.expect("paired models JSON");
    let paired_rows = paired_body["data"]
        .as_array()
        .expect("paired models data array");
    assert!(!paired_rows.is_empty(), "{paired_body}");
    for row in paired_rows {
        assert!(
            row.get("source_id").is_some() && row.get("runtime_id").is_some(),
            "the M3 listing is the extended shape: {row}"
        );
    }

    // 3. Separate ledgers. The pairing token's durable quota is now spent
    //    (`max_requests: 1`), so its next request is refused — while the legacy
    //    token, whose budget lives in a different limiter entirely, keeps working
    //    on the same socket. A single shared ledger would fail one of these two.
    let exhausted = models(&paired_token)
        .await
        .expect("second paired models request");
    assert_eq!(
        exhausted.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "the pairing store's own limiter must debit the paired token"
    );
    let exhausted_body: Value = exhausted.json().await.expect("paired 429 JSON");
    assert_eq!(exhausted_body["error"]["code"], "rate_limited");
    let legacy_after = models(legacy_token)
        .await
        .expect("legacy request after the paired quota is spent");
    assert_eq!(
        legacy_after.status(),
        reqwest::StatusCode::OK,
        "exhausting the pairing quota must not refuse legacy traffic"
    );

    // 4. An invalid token of either family is refused, and within the legacy
    //    family "expired" and "never existed" are byte-identical — no existence
    //    oracle. (`server.rs::authenticate_credential` reaches its generic error
    //    by `break`, not by an early return, for exactly this reason.)
    let unknown = models(unknown_legacy_token)
        .await
        .expect("unknown legacy token request");
    let unknown_status = unknown.status();
    let unknown_cors = unknown
        .headers()
        .get("access-control-allow-origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let unknown_bytes = unknown.bytes().await.expect("unknown legacy 401 body");
    let expired = models(expired_legacy_token)
        .await
        .expect("expired legacy token request");
    let expired_status = expired.status();
    let expired_cors = expired
        .headers()
        .get("access-control-allow-origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let expired_bytes = expired.bytes().await.expect("expired legacy 401 body");
    assert_eq!(unknown_status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(expired_status, unknown_status);
    assert_eq!(expired_bytes, unknown_bytes);
    assert_eq!(expired_cors, unknown_cors);

    // The pairing family refuses an unknown token with its own 401 envelope.
    let unknown_paired = models(&unknown_paired_token)
        .await
        .expect("unknown paired token request");
    assert_eq!(unknown_paired.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unknown_paired_body: Value = unknown_paired.json().await.expect("paired 401 JSON");
    assert_eq!(unknown_paired_body["error"]["code"], "unauthorized");
    assert_eq!(
        unknown_paired_body["error"]["type"],
        "little_monkey_m3_error"
    );

    // And the pairing family closes the same oracle the legacy family does: an
    // expired paired token is byte-identical to an unknown one, rather than the
    // `403 "token is expired"` it used to answer, which told a caller the token
    // it holds was once real. The decision is `compatibility_hub.rs`'s
    // `credential_validity_denial`, shared by `preflight_credential` and
    // `authorize_validated`; scope denials on a *live* token keep their own 403
    // (the caller already proved possession), which is why this collapse is
    // limited to credential validity.
    let expired_paired = models(&expired_paired_token)
        .await
        .expect("expired paired token request");
    let expired_paired_status = expired_paired.status();
    let expired_paired_cors = expired_paired
        .headers()
        .get("access-control-allow-origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let expired_paired_bytes = expired_paired
        .bytes()
        .await
        .expect("expired paired 401 body");
    let unknown_paired_again = models(&unknown_paired_token)
        .await
        .expect("unknown paired token request, for byte comparison");
    let unknown_paired_status = unknown_paired_again.status();
    let unknown_paired_cors = unknown_paired_again
        .headers()
        .get("access-control-allow-origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let unknown_paired_bytes = unknown_paired_again
        .bytes()
        .await
        .expect("unknown paired 401 body");
    assert_eq!(expired_paired_status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(expired_paired_status, unknown_paired_status);
    assert_eq!(expired_paired_bytes, unknown_paired_bytes);
    assert_eq!(expired_paired_cors, unknown_paired_cors);

    server.abort();
    let _ = server.await;
}

// ---------------------------------------------------------------------------
// Roadmap K16: which accelerator backends a harness route actually proves.
// ---------------------------------------------------------------------------

/// The Ollama model the accelerator route below sends. Its presence is the
/// second gate, after `#[ignore]`: ordinary CI has neither a GPU nor an Ollama
/// daemon, so the route must be asked for twice before it runs.
const HARNESS_OLLAMA_MODEL_ENV: &str = "LITTLE_MONKEY_HARNESS_OLLAMA_MODEL";

/// The daemon the accelerator route drives. Loopback, and the same default
/// `m3_production` wires in production, so the route exercises the shipped
/// endpoint rather than a test-only one.
const REAL_OLLAMA_RUNTIME_ID: &str = "ollama";
const REAL_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

/// Named once so [`harness_route`] and the route itself cannot drift.
const REAL_ACCELERATOR_ROUTE: &str = "real_ollama_completion_lands_on_a_detected_accelerator";

/// The compatibility-harness route that proves work reaches `kind`, or `None`
/// when nothing in this app executes on it.
///
/// Exhaustive on `AcceleratorKind` on purpose: a seventh backend cannot be
/// added without saying which of K16's two states it is *here*, in the file
/// that holds the routes, and not only in `execution_support`.
fn harness_route(kind: AcceleratorKind) -> Option<&'static str> {
    match kind {
        // Every other route in this file. They mock at the runtime-driver
        // boundary, so what they prove executes is the HTTP, auth, translation,
        // and hub path — all of it on the host CPU, which is the only backend
        // a runner without a GPU can make a claim about.
        AcceleratorKind::Cpu => Some("every mocked route in this file, on the host CPU"),
        // The one route that requires a detected accelerator and real work.
        AcceleratorKind::Metal | AcceleratorKind::Cuda => Some(REAL_ACCELERATOR_ROUTE),
        AcceleratorKind::Rocm
        | AcceleratorKind::Vulkan
        | AcceleratorKind::DirectMl
        | AcceleratorKind::AppleNeuralEngine => None,
    }
}

/// K16's acceptance, restated where the routes live: for each backend, either a
/// runtime path that executes on it **with a passing compatibility-harness
/// route**, or a stated reason it is detection-only.
///
/// `runtime_adapter.rs`'s `every_backend_either_executes_or_says_why_it_does_not`
/// pins that `execution_support` answers at all. This pins the parenthetical —
/// that an `Executes` answer is backed by a route in this file, and that a
/// `DetectionOnly` answer is not quietly contradicted by one. Runs in ordinary
/// CI; it reads two `&'static str` tables and touches no hardware.
#[test]
fn every_accelerator_backend_has_a_harness_route_or_a_stated_reason() {
    for kind in [
        AcceleratorKind::Cpu,
        AcceleratorKind::Metal,
        AcceleratorKind::Cuda,
        AcceleratorKind::Rocm,
        AcceleratorKind::Vulkan,
        AcceleratorKind::DirectMl,
        AcceleratorKind::AppleNeuralEngine,
    ] {
        match (execution_support(kind), harness_route(kind)) {
            (ExecutionSupport::Executes { via }, Some(route)) => {
                assert!(
                    !via.trim().is_empty(),
                    "{kind:?} claims execution without naming what runs it"
                );
                assert!(
                    !route.trim().is_empty(),
                    "{kind:?} names an empty harness route"
                );
            }
            (ExecutionSupport::DetectionOnly { reason }, None) => assert!(
                reason.len() > 20,
                "{kind:?} is detection-only without saying why"
            ),
            (ExecutionSupport::Executes { via }, None) => panic!(
                "{kind:?} executes via {via}, but no compatibility-harness route proves it; \
                 add one or downgrade it to ExecutionSupport::DetectionOnly"
            ),
            (ExecutionSupport::DetectionOnly { reason }, Some(route)) => {
                panic!("{kind:?} is detection-only ({reason}), but {route} claims to exercise it")
            }
        }
    }
}

/// The real Ollama driver, built exactly as `m3_production::build_ollama_driver`
/// builds it, minus the structured-output capability wrapper that route does not
/// use. Nothing here is a mock: a real HTTP transport against the real daemon.
fn real_ollama_driver(platform: PlatformCapabilities) -> Arc<dyn M3RuntimeDriver> {
    let transport: Arc<dyn HttpTransport> =
        Arc::new(ReqwestHttpTransport::new().expect("real ollama http transport"));
    let adapter: Arc<dyn RuntimeAdapter> = Arc::new(
        OllamaHttpAdapter::new(
            REAL_OLLAMA_RUNTIME_ID,
            REAL_OLLAMA_ENDPOINT,
            EndpointPolicy::LoopbackOnly,
            transport,
            platform,
        )
        .expect("real ollama adapter"),
    );
    let inference: Arc<dyn M3InferenceEngine> = Arc::new(
        OpenAiCompatibleM3InferenceEngine::new(REAL_OLLAMA_ENDPOINT)
            .expect("real ollama inference engine"),
    );
    Arc::new(RuntimeAdapterM3Driver::new(adapter, inference).expect("real ollama driver"))
}

/// The accelerator route: a real `/v1/chat/completions` through the real
/// `m3_http_server` listener, into a real Ollama daemon, on a machine whose own
/// detector reports Metal or CUDA — and then the residency the daemon reports
/// back, which is the only evidence available that the work did not fall to CPU.
///
/// **What this proves and what it cannot.** It proves *an* accelerator ran the
/// completion: detection says the machine has one, and `RunningModel::vram_bytes`
/// is nonzero, which a CPU-resident model never reports. It does not prove
/// *which* one, and no assertion here should imply otherwise — nothing in
/// Ollama's HTTP surface names a device, which is the same limit K15 hit when a
/// per-device split had to answer `UnsupportedCapability`. A per-backend claim
/// needs a runtime whose API names the card.
///
/// **Why it is gated twice.** Hosted CI has no GPU and no Ollama, and this repo
/// cannot make a per-machine hardware claim on a runner it does not own. Running
/// it needs a native or self-hosted machine with a Metal or CUDA device, a
/// running `ollama serve` on `127.0.0.1:11434`, and a small model already
/// pulled:
///
/// ```sh
/// LITTLE_MONKEY_HARNESS_OLLAMA_MODEL=llama3.2:1b \
///   cargo test --test m3_compatibility_harness -- --ignored
/// ```
#[tokio::test]
#[ignore = "needs a real Ollama daemon on a machine with a detected Metal or CUDA device; set LITTLE_MONKEY_HARNESS_OLLAMA_MODEL"]
async fn real_ollama_completion_lands_on_a_detected_accelerator() {
    let model = std::env::var(HARNESS_OLLAMA_MODEL_ENV).unwrap_or_else(|_| {
        panic!("{HARNESS_OLLAMA_MODEL_ENV} must name a model already pulled into the local Ollama")
    });

    // Real detection, not `TestHardware`. The route is a claim about this
    // machine, so the fixture cannot be the one making it.
    let snapshot = SystemM3HardwareProbe
        .snapshot()
        .expect("real hardware snapshot");
    let accelerator = snapshot
        .platform
        .accelerators
        .iter()
        .find(|entry| {
            entry.available && matches!(entry.kind, AcceleratorKind::Metal | AcceleratorKind::Cuda)
        })
        .unwrap_or_else(|| {
            panic!("{REAL_ACCELERATOR_ROUTE} needs a detected Metal or CUDA device; this machine reports none")
        })
        .clone();
    assert!(
        execution_support(accelerator.kind).executes(),
        "{:?} is detected but execution_support says nothing executes on it, so this route is \
         asserting something the app does not claim",
        accelerator.kind
    );

    let root = TestDirectory::new("real-accelerator");
    let hub = hub_with(
        &root,
        vec![real_ollama_driver(snapshot.platform.clone())],
        Arc::new(SystemM3HardwareProbe),
    );
    // Reach the daemon before asking the route to. `/v1/chat/completions`
    // answers a runtime that cannot be listed with one opaque
    // `model_source_unavailable`, which reads identically whether the daemon is
    // down, the model was never pulled, or the completion itself failed — three
    // different bugs behind one 502.
    let inventory = hub
        .runtime_inventory(REAL_OLLAMA_RUNTIME_ID, &M3OperationContext::new(30_000))
        .await
        .unwrap_or_else(|error| {
            panic!("{REAL_OLLAMA_ENDPOINT} did not answer an inventory request: {error:?}")
        });
    assert!(
        inventory.models.iter().any(
            |entry| entry.model_id == model || entry.model_id.starts_with(&format!("{model}:"))
        ),
        "{model} is not pulled into the daemon at {REAL_OLLAMA_ENDPOINT}; it reports: {:?}",
        inventory
            .models
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect::<Vec<_>>()
    );

    let Some((state, base)) = start_test_server(hub.clone()).await else {
        return;
    };

    let response = http_client()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-little-monkey-runtime-id", REAL_OLLAMA_RUNTIME_ID)
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
            // Generous on purpose: a reasoning-tuned model spends its first
            // tokens thinking, and a truncated reply arrives as empty content,
            // which the translation layer correctly refuses as a malformed
            // response. Sixteen tokens tests the model's brevity, not the route.
            "max_tokens": 256,
            "stream": false
        }))
        .send()
        .await
        .expect("real ollama chat completion request");
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .expect("real ollama chat completion JSON");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "real completion failed: {payload}"
    );
    assert_eq!(payload["object"], "chat.completion");
    assert!(
        !payload["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "a successful completion produced no text: {payload}"
    );

    // The evidence. Ollama reports per-model residency through the same adapter
    // status view the Runtime Hub reads, and a model it placed on the CPU
    // reports `vram_bytes` of zero.
    let view = hub
        .runtime_status(REAL_OLLAMA_RUNTIME_ID, &M3OperationContext::new(30_000))
        .await
        .expect("real ollama runtime status");
    let M3RuntimeStatusView::Adapter {
        running_models: resident,
        ..
    } = view
    else {
        panic!("an adapter-backed runtime must report the adapter status view");
    };
    let loaded = resident
        .iter()
        .find(|entry| entry.model_id == model || entry.model_id.starts_with(&format!("{model}:")))
        .unwrap_or_else(|| {
            panic!("{model} is not resident after a successful completion; running: {resident:?}")
        });
    assert!(
        loaded.vram_bytes > 0,
        "{model} answered but reports no VRAM, so the completion ran on the CPU despite {:?} \
         being detected: {loaded:?}",
        accelerator.kind
    );

    state.stop().await;
}
