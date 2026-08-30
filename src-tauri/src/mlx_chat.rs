//! Loopback OpenAI-compatible endpoint in front of the MLX runtime.
//!
//! # Why this exists at all
//!
//! Chat in this app talks to one shape of thing: an unauthenticated
//! OpenAI-compatible HTTP endpoint on `127.0.0.1`. That is what
//! `llama-server` is, and `targetRouting.ts` resolves every local turn — chat,
//! tools, side tasks, compare, crew — to `http://127.0.0.1:<port>`.
//!
//! The MLX service is not that. It speaks a private `POST /v1/generate`
//! newline-delimited protocol (see `ProductionMlxServiceController`), and the
//! translation from OpenAI's wire format to it already exists inside the
//! runtime hub — `M3RuntimeHub::dispatch_api_stream` takes an OpenAI body and
//! drives the MLX driver with it, tools and all.
//!
//! So this module is deliberately thin: a loopback listener that hands a
//! request body to the hub and writes the frames the hub produces back out as
//! SSE. Everything that makes an MLX turn work — protocol translation,
//! starting the service, cancellation, tool calls — is the hub's, not this
//! file's. What this file buys is that no other part of the app has to learn
//! that MLX exists.
//!
//! # Trust
//!
//! Bound to `127.0.0.1` on an ephemeral port, exactly like the `llama-server`
//! the app already runs unauthenticated on loopback, and it exposes strictly
//! one model: the body's `model` field is overwritten with the model this
//! listener was started for, so a caller cannot reach any other model — or any
//! other runtime — through it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::compatibility_hub::{CompatibilityProtocol, ProtocolStreamFrame};
use crate::m3_runtime_hub::{
    M3ApiCaller, M3ApiDispatchRequest, M3OperationContext, M3ProtocolFrameSink, M3RuntimeHub,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ResponseBody = BoxBody<Bytes, BoxError>;

/// The MLX runtime's id in the hub. There is exactly one.
const MLX_RUNTIME_ID: &str = "mlx";
/// Bounded so a local caller cannot make this listener allocate without limit.
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// How long one chat turn may take, start to finish.
///
/// The hub's own default is two minutes, which is the right bound for a control
/// operation and the wrong one here: the first request against a model that is
/// not yet resident pays for loading the weights, and a 20GB safetensors
/// checkout does not load in two minutes. Fifteen minutes is the ceiling
/// `mlx_runtime::validate_context` enforces, so this sits just inside it — ask
/// for more and every turn is refused before it starts.
///
/// It stays bounded rather than unbounded because a wedged service should
/// eventually report something. A client that gives up sooner — the Stop button
/// — closes the connection, which drops the frame receiver and ends the stream
/// on its own, so this ceiling is the backstop and not how a turn normally ends.
const CHAT_TIMEOUT_MS: u64 = 14 * 60 * 1_000;

/// Exactly what `compatibility_hub::translate_openai_chat` accepts. Kept beside
/// the code that needs it rather than exported from there: this list is a
/// statement about what this endpoint forwards, and it should not silently grow
/// because the translator learned a new field.
const CHAT_COMPLETION_FIELDS: [&str; 9] = [
    "model",
    "messages",
    "tools",
    "stream",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "response_format",
    "metadata",
];

/// What the frontend needs to point a chat turn at a running MLX model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MlxChatStatus {
    pub running: bool,
    pub port: u16,
    pub model_id: String,
    pub model_path: String,
    /// Whether the running model can read images, from its own `config.json`.
    /// The chat UI offers an attachment on this, the way it does on
    /// `llama_status.vision_enabled` for a GGUF with a projector.
    pub vision: bool,
}

impl MlxChatStatus {
    pub fn stopped() -> Self {
        Self {
            running: false,
            port: 0,
            model_id: String::new(),
            model_path: String::new(),
            vision: false,
        }
    }
}

struct RunningServer {
    port: u16,
    model_id: String,
    model_path: String,
    vision: bool,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

/// The one MLX chat endpoint this process runs.
///
/// One, not one per model: the MLX runtime itself holds a single resident model
/// (`MlxRuntimeAdapter` starts one service process), so a second listener could
/// only ever contend for it.
#[derive(Default)]
pub struct MlxChatState {
    running: Mutex<Option<RunningServer>>,
}

impl MlxChatState {
    pub fn status(&self) -> MlxChatStatus {
        match self.running.lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|server| MlxChatStatus {
                    running: true,
                    port: server.port,
                    model_id: server.model_id.clone(),
                    model_path: server.model_path.clone(),
                    vision: server.vision,
                })
                .unwrap_or_else(MlxChatStatus::stopped),
            Err(_) => MlxChatStatus::stopped(),
        }
    }

    /// Stops the listener, if one is running. The MLX service process itself is
    /// the runtime hub's to stop; this only closes the door in front of it.
    pub fn stop(&self) -> Result<(), String> {
        let previous = self
            .running
            .lock()
            .map_err(|_| "MLX chat endpoint lock poisoned".to_string())?
            .take();
        if let Some(mut server) = previous {
            if let Some(shutdown) = server.shutdown.take() {
                let _ = shutdown.send(());
            }
            server.task.abort();
        }
        Ok(())
    }

    fn install(&self, server: RunningServer) -> Result<(), String> {
        let mut guard = self
            .running
            .lock()
            .map_err(|_| "MLX chat endpoint lock poisoned".to_string())?;
        if let Some(mut previous) = guard.take() {
            if let Some(shutdown) = previous.shutdown.take() {
                let _ = shutdown.send(());
            }
            previous.task.abort();
        }
        *guard = Some(server);
        Ok(())
    }
}

/// Starts the endpoint for `model_id`, replacing any previous one.
///
/// Returns once the socket is bound, so the caller can hand the port straight
/// to a chat turn. The MLX service process starts lazily on the first request —
/// that is the hub driver's own behaviour, and it means a model that fails to
/// load reports the failure as a stream error the user can read rather than as
/// a silent non-start here.
pub async fn start(
    state: &MlxChatState,
    hub: Arc<M3RuntimeHub>,
    model_id: String,
    model_path: String,
    tool_calling: bool,
    vision: bool,
) -> Result<MlxChatStatus, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .map_err(|error| format!("Failed to bind the MLX chat endpoint: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Failed to read the MLX chat endpoint port: {error}"))?
        .port();
    let (shutdown_sender, mut shutdown_receiver) = oneshot::channel();
    let served = ServedModel {
        id: model_id.clone(),
        tool_calling,
    };
    let task = tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                _ = &mut shutdown_receiver => return,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    // A single failed accept is not a reason to take the
                    // endpoint down; the next one usually succeeds.
                    Err(_) => continue,
                },
            };
            let hub = hub.clone();
            let served = served.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let hub = hub.clone();
                    let served = served.clone();
                    async move { Ok::<_, Infallible>(route(hub, served, request).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    state.install(RunningServer {
        port,
        model_id: model_id.clone(),
        model_path: model_path.clone(),
        vision,
        shutdown: Some(shutdown_sender),
        task,
    })?;
    Ok(MlxChatStatus {
        running: true,
        port,
        model_id,
        model_path,
        vision,
    })
}

/// The single model this endpoint serves, and what it can do.
#[derive(Clone)]
struct ServedModel {
    id: String,
    /// Whether the model's own chat template advertises tool calling, recorded
    /// at install time. The MLX runtime refuses a request carrying tools for a
    /// model without it, so this decides whether tools are forwarded.
    tool_calling: bool,
}

async fn route(
    hub: Arc<M3RuntimeHub>,
    served: ServedModel,
    request: Request<Incoming>,
) -> Response<ResponseBody> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    match (&method, path.as_str()) {
        // `/health` and `/v1/models` are what a client probes before it commits
        // to a turn, and both are answerable without touching the runtime.
        (&Method::GET, "/health") => json_response(StatusCode::OK, json!({"status": "ok"})),
        (&Method::GET, "/v1/models") => json_response(
            StatusCode::OK,
            json!({
                "object": "list",
                "data": [{"id": served.id, "object": "model", "owned_by": "little-monkey"}],
            }),
        ),
        (&Method::POST, "/v1/chat/completions") => {
            let body = match read_body(request).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            chat_completions(hub, &served, body).await
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            json!({"error": {"message": "unsupported route", "type": "invalid_request_error"}}),
        ),
    }
}

async fn read_body(request: Request<Incoming>) -> Result<Bytes, Response<ResponseBody>> {
    let upper = request.body().size_hint().upper().unwrap_or(u64::MAX);
    if upper > MAX_REQUEST_BYTES {
        return Err(json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": {"message": "request body is too large", "type": "invalid_request_error"}}),
        ));
    }
    request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|error| {
            json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("unreadable request body: {error}"), "type": "invalid_request_error"}}),
            )
        })
}

async fn chat_completions(
    hub: Arc<M3RuntimeHub>,
    served: &ServedModel,
    body: Bytes,
) -> Response<ResponseBody> {
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("invalid JSON body: {error}"), "type": "invalid_request_error"}}),
            )
        }
    };
    let Some(object) = parsed.as_object_mut() else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "body must be a JSON object", "type": "invalid_request_error"}}),
        );
    };
    // This endpoint serves exactly one model. Callers built for `llama-server`
    // send whatever name they like — it ignores the field — so rather than
    // reject them, name the model they actually reached.
    let streaming = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    object.insert("model".to_string(), Value::String(served.id.clone()));
    // The MLX runtime rejects a request carrying tools for a model whose chat
    // template never advertised them. This app sends its tool set on every
    // agent turn, so honouring that rejection would make such a model unusable
    // rather than merely tool-less. Drop the tools instead: the model then
    // answers in prose, which is exactly what a non-tool GGUF does on the
    // llama.cpp path.
    if !served.tool_calling {
        object.remove("tools");
    }
    // The hub's translator refuses a body carrying a field it does not model,
    // which is the right call for an inbound API but the wrong one here: this
    // endpoint stands in for `llama-server`, and the app's own client sends two
    // fields llama.cpp honours and the translator does not name —
    // `stream_options.include_usage` and `tool_choice: "auto"`. Neither changes
    // what the model is asked to do (usage arrives in the final chunk either
    // way, and automatic tool choice is already the canonical default), so drop
    // them rather than fail a turn over them.
    object.retain(|key, _| CHAT_COMPLETION_FIELDS.contains(&key.as_str()));

    let encoded = match serde_json::to_vec(&parsed) {
        Ok(encoded) => encoded,
        Err(error) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": format!("could not re-encode request: {error}"), "type": "little_monkey_error"}}),
            )
        }
    };
    let dispatch = M3ApiDispatchRequest {
        protocol: CompatibilityProtocol::OpenAiChatCompletions,
        runtime_id: MLX_RUNTIME_ID.to_string(),
        request_id: format!("mlx-{}", Uuid::new_v4()),
        body: encoded,
        caller: M3ApiCaller::Internal,
        now_ms: now_ms(),
    };
    let context = M3OperationContext::new(CHAT_TIMEOUT_MS);

    if !streaming {
        return match hub.dispatch_api(&dispatch, &context).await {
            Ok(response) => json_response(
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK),
                response.body,
            ),
            Err(error) => json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": error.to_string(), "type": "little_monkey_m3_error"}}),
            ),
        };
    }

    let (sender, receiver) = mpsc::channel(64);
    let mut sink = ChannelFrameSink {
        sender: sender.clone(),
    };
    tokio::spawn(async move {
        if let Err(error) = hub
            .dispatch_api_stream(&dispatch, &mut sink, &context)
            .await
        {
            // The response has already committed to 200 by the time a stream
            // can fail, so a failure has to arrive as a frame the client can
            // surface rather than as a status code.
            let frame = ProtocolStreamFrame {
                event: Some("error".to_string()),
                data: json!({
                    "error": {
                        "message": error.to_string(),
                        "type": "little_monkey_m3_error"
                    }
                })
                .to_string(),
            };
            let _ = sender.try_send(Bytes::from(frame.to_sse_bytes()));
        }
    });
    sse_response(receiver)
}

struct ChannelFrameSink {
    sender: mpsc::Sender<Bytes>,
}

impl M3ProtocolFrameSink for ChannelFrameSink {
    fn emit(&mut self, frame: ProtocolStreamFrame) -> Result<(), String> {
        self.sender
            .try_send(Bytes::from(frame.to_sse_bytes()))
            .map_err(|error| format!("MLX chat client is disconnected or too slow: {error}"))
    }
}

fn sse_response(receiver: mpsc::Receiver<Bytes>) -> Response<ResponseBody> {
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<Frame<Bytes>, Infallible>(Frame::data(bytes)), receiver))
    });
    let body = StreamBody::new(stream)
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("fixed MLX SSE response is valid")
}

/// Wall-clock milliseconds for the hub's rate/quota accounting. The hub only
/// ever compares this against its own recorded timestamps, so a clock before
/// the epoch is reported as zero rather than as an error nobody can act on.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn json_response(status: StatusCode, body: Value) -> Response<ResponseBody> {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            Full::new(Bytes::from(bytes))
                .map_err(|never: Infallible| -> BoxError { match never {} })
                .boxed(),
        )
        .expect("fixed MLX JSON response is valid")
}

/// Starts the MLX runtime for the managed bundle at `model_path` and returns
/// the loopback endpoint a chat turn should use.
///
/// Studio's video engine and this share one MLX memory slot, so the same
/// handoff `m3_runtime_load_model` performs happens here: take the
/// application-wide ownership guard, stop Studio's engine, and only then hand
/// the slot to chat.
#[tauri::command]
pub async fn mlx_chat_start(
    state: tauri::State<'_, crate::AppState>,
    m3: tauri::State<'_, crate::m3_commands::M3CommandState>,
    model_path: String,
) -> Result<MlxChatStatus, String> {
    let path = std::path::PathBuf::from(&model_path);
    let provenance = crate::model_sources::load_bundle_provenance(&path)?
        .ok_or_else(|| format!("{model_path} is not an app-owned model bundle"))?;
    if provenance.runtime != crate::model_sources::ModelRuntimeKind::Mlx {
        return Err(format!("{model_path} is not an MLX model"));
    }
    crate::model_sources::verify_bundle_for_runtime(&path)?;

    let _owner = m3.mlx_ownership.acquire().await;
    state.generation_engine.stop()?;
    // A bundle installed since the drivers were last built is not in the MLX
    // adapter's model map yet, and the driver refuses a model it has not
    // reconciled. Refreshing here is what makes "install, then start" work
    // without a restart.
    let context = M3OperationContext::default();
    m3.hub
        .refresh_runtimes(&context)
        .await
        .map_err(|error| format!("Failed to refresh local runtimes: {error}"))?;

    start(
        &state.mlx_chat,
        m3.hub.clone(),
        provenance.local_dir_name.clone(),
        model_path,
        provenance.tool_calling,
        provenance.vision,
    )
    .await
}

/// The running MLX chat endpoint, or a stopped one.
#[tauri::command]
pub fn mlx_chat_status(state: tauri::State<'_, crate::AppState>) -> MlxChatStatus {
    state.mlx_chat.status()
}

/// Closes the endpoint and unloads the MLX service process behind it.
#[tauri::command]
pub async fn mlx_chat_stop(
    state: tauri::State<'_, crate::AppState>,
    m3: tauri::State<'_, crate::m3_commands::M3CommandState>,
) -> Result<(), String> {
    let status = state.mlx_chat.status();
    state.mlx_chat.stop()?;
    if !status.running {
        return Ok(());
    }
    let _owner = m3.mlx_ownership.acquire().await;
    let context = M3OperationContext::default();
    m3.hub
        .unload_model(
            &crate::m3_runtime_hub::M3UnloadModelRequest {
                runtime_id: MLX_RUNTIME_ID.to_string(),
                model_id: status.model_id,
                force_exact_owner: false,
            },
            &context,
        )
        .await
        .map_err(|error| format!("Failed to unload the MLX model: {error}"))
}

// The end-to-end test drives the real MLX runtime, which exists only in the
// macOS build. The module above is deliberately not gated — it names no MLX
// type — but this is.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The whole path, on real weights and the real MLX runtime: install a
    /// safetensors repository, point the hub at it, start this endpoint, and
    /// stream an OpenAI chat completion through it.
    ///
    /// Ignored by default. It needs an Apple Silicon machine, network access,
    /// ~80MB of transfer, and an installed MLX runtime package **built from the
    /// current `packaging/mlx/service/mlx_server.py`** — a package predating the
    /// generation worker in that file runs `stream_generate` on a request
    /// thread, fails every generation with "There is no Stream(gpu, 0) in
    /// current thread.", and this test reports it as missing text.
    ///
    /// `cargo test --lib mlx_chat -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires an installed MLX runtime, network access and ~80MB"]
    async fn an_installed_mlx_model_answers_an_openai_chat_completion() {
        let root = std::env::temp_dir().join(format!("little-monkey-mlx-chat-{}", Uuid::new_v4()));
        let models_dir = root.join("models");
        std::fs::create_dir_all(&models_dir).unwrap();

        // The signed MLX runtime package is installed per profile by the app,
        // not by this test. Clone the installed one into the temporary root so
        // the test exercises the real service without touching the real
        // profile. `cp -c` is an APFS clone: no second copy of the bytes.
        let installed_root = dirs::data_dir()
            .expect("a data directory")
            .join("com.littlemonkey.app")
            .join("m3")
            .join("runtimes");
        if !installed_root.join("mlx").join("active.json").is_file() {
            eprintln!("no MLX runtime installed for this profile — skipping");
            return;
        }
        std::fs::create_dir_all(root.join("m3").join("runtimes")).unwrap();
        let cloned = std::process::Command::new("cp")
            .arg("-Rc")
            .arg(installed_root.join("mlx"))
            .arg(root.join("m3").join("runtimes").join("mlx"))
            .status()
            .expect("cp runs");
        assert!(cloned.success(), "cloning the MLX runtime failed");

        // The smallest real MLX chat model on the hub — the point is the path,
        // not the weights, and ~80MB keeps the test runnable anywhere.
        let reference = "https://huggingface.co/mlx-community/SmolLM-135M-Instruct-4bit/tree/main";
        let resolved = crate::model_sources::resolve_reference(reference)
            .await
            .expect("the repository resolves");
        crate::model_sources::install_reference(&models_dir, reference, &resolved.sha256, |_| {})
            .await
            .expect("the repository installs");

        let hub = crate::m3_production::mlx_only_hub_for_tests(&root)
            .expect("an MLX-only hub builds against the temporary root");
        let bundles = crate::model_sources::installed_bundles(&models_dir);
        let bundle = bundles.first().expect("one installed bundle");
        let model_id = bundle.provenance.local_dir_name.clone();

        let endpoint = MlxChatState::default();
        // SmolLM's chat template does not advertise tools, so this also covers
        // the case the app hits constantly: an agent turn carrying a tool set
        // for a model that cannot use one must still answer.
        assert!(!bundle.provenance.tool_calling);
        let status = start(
            &endpoint,
            hub.clone(),
            model_id.clone(),
            bundle.path.to_string_lossy().to_string(),
            bundle.provenance.tool_calling,
            bundle.provenance.vision,
        )
        .await
        .expect("the endpoint binds");
        assert!(status.running && status.port > 0);

        let client = reqwest::Client::new();
        // A client built for `llama-server` names whatever model it likes; the
        // endpoint serves the one it was started for.
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                status.port
            ))
            // Byte-for-byte the body shape `llamaClient.ts` sends, including the
            // two fields the hub's translator does not name. A turn from the
            // real app must not be refused over them.
            .json(&json!({
                "model": "local",
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 24,
                "tool_choice": "auto",
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read a file",
                        "parameters": {"type": "object", "properties": {}},
                    },
                }],
                "messages": [{"role": "user", "content": "Reply with the single word: ready"}],
            }))
            .send()
            .await
            .expect("the endpoint answers");
        assert_eq!(response.status(), 200);
        let body = response.text().await.expect("a readable stream");

        // Real content, not just a well-formed empty stream.
        let text = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|payload| *payload != "[DONE]")
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .filter_map(|value| {
                value["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<String>();
        assert!(
            !text.trim().is_empty(),
            "expected generated text, got: {body}"
        );
        assert!(body.contains("[DONE]"), "stream did not terminate: {body}");

        endpoint.stop().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
