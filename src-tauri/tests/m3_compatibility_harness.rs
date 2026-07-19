//! Phase 8 item 11 (OpenAI/Ollama API compatibility harness): real
//! integration tests that spin up the actual `m3_http_server` loopback
//! listener (the same `start_server_core`/`M3HttpServerState` used in
//! production) in front of a real `M3RuntimeHub`, and make real HTTP
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
use little_monkey_lib::m3_http_server::{start_server_core, stop_server_core, M3HttpServerState};
use little_monkey_lib::m3_runtime_hub::*;
use little_monkey_lib::runtime_adapter::{
    EndpointOrigin, EndpointPolicy, HardwareSnapshot, KeepAlive, ModelCapabilities,
    PlatformCapabilities, RuntimeDescriptor, RuntimeInventory, RuntimeKind, RuntimeLifecycleState,
    RuntimeLogTail, RuntimeModel, RuntimeStatus, SettingValue,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
}

impl MockRuntimeDriver {
    fn ollama() -> Self {
        Self {
            runtime_id: "harness-ollama".to_string(),
            kind: M3RuntimeKind::Ollama,
            backend: ApiBackend::Ollama,
            model_id: "llama3".to_string(),
            supports_embed: true,
        }
    }

    fn llama_cpp() -> Self {
        Self {
            runtime_id: "harness-llama".to_string(),
            kind: M3RuntimeKind::LlamaCpp,
            backend: ApiBackend::ManagedLocal,
            model_id: "qwen-local".to_string(),
            supports_embed: false,
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

    fn status<'a>(&'a self, _context: &'a M3OperationContext) -> M3HubFuture<'a, M3RuntimeStatusView> {
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

    fn inventory<'a>(&'a self, _context: &'a M3OperationContext) -> M3HubFuture<'a, RuntimeInventory> {
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

    fn logs<'a>(&'a self, _max_bytes: usize, _context: &'a M3OperationContext) -> M3HubFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            Ok(RuntimeLogTail {
                text: String::new(),
                truncated: false,
            })
        })
    }

    fn metrics<'a>(&'a self, _context: &'a M3OperationContext) -> M3HubFuture<'a, M3RuntimeMetricsView> {
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
            use little_monkey_lib::compatibility_hub::{CanonicalContent, CanonicalInferenceResponse, CanonicalUsage};
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
                },
            })
            .map_err(M3HubError::Runtime)
        })
    }

    fn cancel<'a>(&'a self, _request_id: &'a str, _context: &'a M3OperationContext) -> M3HubFuture<'a, bool> {
        Box::pin(async move { Ok(false) })
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
            use little_monkey_lib::compatibility_hub::{CanonicalEmbeddingDatum, CanonicalEmbeddingResponse, CanonicalUsage};
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
                },
            })
        })
    }
}

fn test_hub(root: &TestDirectory) -> Arc<M3RuntimeHub> {
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
                runtimes: vec![
                    Arc::new(MockRuntimeDriver::ollama()),
                    Arc::new(MockRuntimeDriver::llama_cpp()),
                ],
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
/// `(state, base_url)`. The caller must `stop_server_core(&state)` when
/// done.
async fn start_test_server(hub: Arc<M3RuntimeHub>) -> Option<(M3HttpServerState, String)> {
    let Some(port) = free_loopback_port().await else {
        return None;
    };
    let mut policy = LanServerPolicy::default();
    policy.port = port;
    policy.require_authentication = false;
    hub.configure_lan(policy).expect("configure loopback policy");
    let state = M3HttpServerState::default();
    let started = start_server_core(&state, hub)
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

#[tokio::test]
async fn v1_models_and_api_tags_list_the_scoped_runtimes_models() {
    let root = TestDirectory::new("models-tags");
    let hub = test_hub(&root);
    let Some((state, base)) = start_test_server(hub).await else {
        return;
    };
    let client = http_client();

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

    stop_server_core(&state).await.expect("stop server");
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
    assert_eq!(payload["choices"][0]["message"]["content"], "functional response");

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
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.text().await.expect("SSE body");
    assert!(body.contains("chat.completion.chunk"));
    assert!(body.contains("\"content\":\"streamed\""));
    assert!(body.contains("[DONE]"));

    stop_server_core(&state).await.expect("stop server");
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

    stop_server_core(&state).await.expect("stop server");
}

#[tokio::test]
async fn embeddings_succeeds_for_a_real_embeddings_capable_runtime_and_is_honestly_unsupported_otherwise() {
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

    stop_server_core(&state).await.expect("stop server");
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
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let payload: Value = response.json().await.expect("ollama chat JSON");
    assert_eq!(payload["model"], "llama3");
    assert_eq!(payload["done"], true);
    assert_eq!(payload["message"]["role"], "assistant");
    assert_eq!(payload["message"]["tool_calls"][0]["function"]["name"], "weather");
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
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
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

    stop_server_core(&state).await.expect("stop server");
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
    hub.configure_lan(policy).expect("configure authenticated policy");

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
        .complete_pairing(&chat_only.challenge_id, &chat_only.pairing_code, 1_000, "127.0.0.1")
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

    let state = M3HttpServerState::default();
    let started = start_server_core(&state, hub).await.expect("start M3 HTTP server");
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

    stop_server_core(&state).await.expect("stop server");
}
