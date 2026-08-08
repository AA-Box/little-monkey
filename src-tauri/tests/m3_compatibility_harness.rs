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
use little_monkey_lib::m3_runtime_hub::*;
use little_monkey_lib::runtime_adapter::{
    EndpointOrigin, EndpointPolicy, HardwareSnapshot, KeepAlive, ModelCapabilities,
    PlatformCapabilities, RuntimeDescriptor, RuntimeInventory, RuntimeKind, RuntimeLifecycleState,
    RuntimeLogTail, RuntimeModel, RuntimeStatus, SettingValue,
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
use std::sync::Arc;
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

async fn free_loopback_port() -> Option<u16> {
    match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => Some(listener.local_addr().expect("ephemeral address").port()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping compatibility harness: sandbox forbids local listeners");
            None
        }
        Err(error) => panic!("bind ephemeral test port: {error}"),
    }
}

/// Starts the real M3 HTTP server (loopback, unauthenticated — a
/// pre-existing, deliberate policy for `127.0.0.1` when
/// `require_authentication` is false) in front of `hub` and returns
/// `(state, base_url)`. The caller must stop the harness when done.
async fn start_test_server(hub: Arc<M3RuntimeHub>) -> Option<(CompatibilityHarnessServer, String)> {
    let Some(port) = free_loopback_port().await else {
        return None;
    };
    let mut policy = LanServerPolicy::default();
    policy.port = port;
    policy.require_authentication = false;
    hub.configure_lan(policy)
        .expect("configure loopback policy");
    let (state, started) = start_compatibility_harness(hub)
        .await
        .expect("start M3 HTTP server");
    assert_eq!(started.status, "running");
    Some((state, format!("http://127.0.0.1:{port}")))
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

    let first = tokio::time::timeout(
        Duration::from_secs(2),
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
    let server = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
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
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .get(format!("{base}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("legacy unified listener readiness");

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

    let server = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
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
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .get(format!("{base}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("unified primary endpoint readiness");

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
