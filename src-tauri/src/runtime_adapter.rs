//! Tauri-free runtime abstraction for local model engines.
//!
//! The adapters in this module deliberately depend on injectable HTTP and
//! process-controller traits.  Production integration can use
//! [`ReqwestHttpTransport`] for Ollama and implement
//! [`ManagedProcessController`] with `std::process::Command`/Tauri's process
//! APIs without ever constructing a shell command string.  Tests use the
//! same contracts with deterministic in-memory transports and controllers.

use futures_util::StreamExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub use tokio_util::sync::CancellationToken;
use url::{Host, Url};

pub const RUNTIME_ADAPTER_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_OPERATION_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_LOG_BYTES: usize = 256 * 1024;
pub const DEFAULT_MAX_CONFIG_BYTES: usize = 128 * 1024;

const ABSOLUTE_MAX_TIMEOUT_MS: u64 = 15 * 60 * 1_000;
const ABSOLUTE_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const ABSOLUTE_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
const ABSOLUTE_MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_MODELS_PER_RESPONSE: usize = 4_096;
const MAX_SETTINGS: usize = 64;
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_SETTING_STRING_BYTES: usize = 64 * 1024;

pub type RuntimeAdapterResult<T> = Result<T, RuntimeAdapterError>;
pub type RuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = RuntimeAdapterResult<T>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeAdapterError {
    InvalidEndpoint {
        endpoint: String,
        message: String,
    },
    InvalidIdentifier {
        field: &'static str,
        value: String,
    },
    InvalidOperationLimits {
        message: String,
    },
    InvalidSetting {
        key: String,
        message: String,
    },
    UnsupportedCapability {
        runtime_id: String,
        capability: String,
    },
    UnsupportedSetting {
        runtime_id: String,
        key: String,
    },
    ConfigTooLarge {
        limit: usize,
        actual: usize,
    },
    ResponseTooLarge {
        limit: usize,
        actual_at_least: usize,
    },
    LogTooLarge {
        limit: usize,
        actual: usize,
    },
    Cancelled {
        operation: String,
    },
    Timeout {
        operation: String,
        timeout_ms: u64,
    },
    Transport {
        operation: String,
        message: String,
    },
    HttpStatus {
        operation: String,
        status: u16,
        body: String,
    },
    MalformedResponse {
        operation: String,
        message: String,
    },
    Controller {
        operation: String,
        message: String,
    },
    ModelNotFound {
        runtime_id: String,
        model_id: String,
    },
    ModelNotRunning {
        runtime_id: String,
        model_id: String,
    },
    ModelPathUnavailable {
        runtime_id: String,
        model_id: String,
    },
    ProcessSlotBusy {
        slot_id: String,
        model_id: String,
    },
    PortCollision {
        port: u16,
        owner_id: String,
    },
    InsufficientMemory {
        target_id: String,
        required_ram_bytes: u64,
        available_ram_bytes: u64,
        required_vram_bytes: u64,
        available_vram_bytes: u64,
    },
    NoCompatibleProcessSlot {
        target_id: String,
        runtime: RuntimeKind,
    },
    IncompatiblePlatform {
        target_id: String,
        accelerator: AcceleratorKind,
    },
    InvalidProcessSpec {
        message: String,
    },
    LockPoisoned,
}

impl fmt::Display for RuntimeAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint { endpoint, message } => {
                write!(f, "invalid runtime endpoint {endpoint:?}: {message}")
            }
            Self::InvalidIdentifier { field, value } => {
                write!(f, "invalid {field}: {value:?}")
            }
            Self::InvalidOperationLimits { message } => {
                write!(f, "invalid runtime operation limits: {message}")
            }
            Self::InvalidSetting { key, message } => {
                write!(f, "invalid runtime setting {key:?}: {message}")
            }
            Self::UnsupportedCapability {
                runtime_id,
                capability,
            } => write!(f, "runtime {runtime_id} does not support {capability}"),
            Self::UnsupportedSetting { runtime_id, key } => {
                write!(f, "runtime {runtime_id} does not support setting {key:?}")
            }
            Self::ConfigTooLarge { limit, actual } => {
                write!(f, "runtime config is {actual} bytes, exceeding {limit}")
            }
            Self::ResponseTooLarge {
                limit,
                actual_at_least,
            } => write!(
                f,
                "runtime response is at least {actual_at_least} bytes, exceeding {limit}"
            ),
            Self::LogTooLarge { limit, actual } => {
                write!(f, "runtime log tail is {actual} bytes, exceeding {limit}")
            }
            Self::Cancelled { operation } => write!(f, "runtime operation {operation} cancelled"),
            Self::Timeout {
                operation,
                timeout_ms,
            } => write!(f, "runtime operation {operation} timed out after {timeout_ms}ms"),
            Self::Transport { operation, message } => {
                write!(f, "runtime transport failed during {operation}: {message}")
            }
            Self::HttpStatus {
                operation,
                status,
                body,
            } => {
                if body.is_empty() {
                    write!(f, "runtime HTTP {operation} failed with status {status}")
                } else {
                    write!(
                        f,
                        "runtime HTTP {operation} failed with status {status}: {body}"
                    )
                }
            }
            Self::MalformedResponse { operation, message } => {
                write!(f, "malformed runtime response during {operation}: {message}")
            }
            Self::Controller { operation, message } => {
                write!(f, "managed runtime controller failed during {operation}: {message}")
            }
            Self::ModelNotFound {
                runtime_id,
                model_id,
            } => write!(f, "model {model_id:?} is not installed in runtime {runtime_id}"),
            Self::ModelNotRunning {
                runtime_id,
                model_id,
            } => write!(f, "model {model_id:?} is not running in runtime {runtime_id}"),
            Self::ModelPathUnavailable {
                runtime_id,
                model_id,
            } => write!(
                f,
                "model {model_id:?} in runtime {runtime_id} has no safe local path"
            ),
            Self::ProcessSlotBusy { slot_id, model_id } => {
                write!(f, "process slot {slot_id} is already serving {model_id}")
            }
            Self::PortCollision { port, owner_id } => {
                write!(f, "runtime port {port} is already owned by {owner_id}")
            }
            Self::InsufficientMemory {
                target_id,
                required_ram_bytes,
                available_ram_bytes,
                required_vram_bytes,
                available_vram_bytes,
            } => write!(
                f,
                "target {target_id} needs {required_ram_bytes} RAM/{required_vram_bytes} VRAM bytes, but only {available_ram_bytes}/{available_vram_bytes} are schedulable"
            ),
            Self::NoCompatibleProcessSlot { target_id, runtime } => write!(
                f,
                "target {target_id} has no available {:?} process slot",
                runtime
            ),
            Self::IncompatiblePlatform {
                target_id,
                accelerator,
            } => write!(
                f,
                "target {target_id} requires unavailable accelerator {:?}",
                accelerator
            ),
            Self::InvalidProcessSpec { message } => {
                write!(f, "invalid managed process specification: {message}")
            }
            Self::LockPoisoned => write!(f, "runtime adapter state lock is poisoned"),
        }
    }
}

impl Error for RuntimeAdapterError {}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOperationLimits {
    pub timeout_ms: u64,
    pub max_response_bytes: usize,
    pub max_log_bytes: usize,
    pub max_config_bytes: usize,
}

impl Default for RuntimeOperationLimits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_OPERATION_TIMEOUT_MS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            max_config_bytes: DEFAULT_MAX_CONFIG_BYTES,
        }
    }
}

impl RuntimeOperationLimits {
    pub fn validate(&self) -> RuntimeAdapterResult<()> {
        let valid = self.timeout_ms > 0
            && self.timeout_ms <= ABSOLUTE_MAX_TIMEOUT_MS
            && self.max_response_bytes > 0
            && self.max_response_bytes <= ABSOLUTE_MAX_RESPONSE_BYTES
            && self.max_log_bytes > 0
            && self.max_log_bytes <= ABSOLUTE_MAX_LOG_BYTES
            && self.max_config_bytes > 0
            && self.max_config_bytes <= ABSOLUTE_MAX_CONFIG_BYTES;
        if valid {
            Ok(())
        } else {
            Err(RuntimeAdapterError::InvalidOperationLimits {
                message: "timeout and byte limits must be non-zero and within hard safety caps"
                    .to_string(),
            })
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeOperationContext {
    pub limits: RuntimeOperationLimits,
    pub cancellation: CancellationToken,
}

impl Default for RuntimeOperationContext {
    fn default() -> Self {
        Self {
            limits: RuntimeOperationLimits::default(),
            cancellation: CancellationToken::new(),
        }
    }
}

impl RuntimeOperationContext {
    pub fn new(limits: RuntimeOperationLimits, cancellation: CancellationToken) -> Self {
        Self {
            limits,
            cancellation,
        }
    }

    pub fn preflight(&self, operation: &str) -> RuntimeAdapterResult<()> {
        self.limits.validate()?;
        if self.cancellation.is_cancelled() {
            Err(RuntimeAdapterError::Cancelled {
                operation: operation.to_string(),
            })
        } else {
            Ok(())
        }
    }
}

async fn bounded_operation<T, F>(
    context: &RuntimeOperationContext,
    operation: &str,
    future: F,
) -> RuntimeAdapterResult<T>
where
    F: Future<Output = RuntimeAdapterResult<T>> + Send,
{
    context.preflight(operation)?;
    let timeout_ms = context.limits.timeout_ms;
    tokio::select! {
        _ = context.cancellation.cancelled() => Err(RuntimeAdapterError::Cancelled {
            operation: operation.to_string(),
        }),
        result = tokio::time::timeout(Duration::from_millis(timeout_ms), future) => {
            match result {
                Ok(result) => result,
                Err(_) => Err(RuntimeAdapterError::Timeout {
                    operation: operation.to_string(),
                    timeout_ms,
                }),
            }
        }
    }
}

async fn async_lock<'a>(
    mutex: &'a tokio::sync::Mutex<()>,
    context: &RuntimeOperationContext,
    operation: &str,
) -> RuntimeAdapterResult<tokio::sync::MutexGuard<'a, ()>> {
    bounded_operation(context, operation, async { Ok(mutex.lock().await) }).await
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointPolicy {
    LoopbackOnly,
    AllowRemoteHttps,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EndpointOrigin {
    origin: String,
    loopback: bool,
}

impl<'de> Deserialize<'de> for EndpointOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EndpointOriginWire {
            origin: String,
            loopback: bool,
        }

        let wire = EndpointOriginWire::deserialize(deserializer)?;
        let policy = if wire.loopback {
            EndpointPolicy::LoopbackOnly
        } else {
            EndpointPolicy::AllowRemoteHttps
        };
        let parsed = EndpointOrigin::parse(&wire.origin, policy)
            .map_err(<D::Error as serde::de::Error>::custom)?;
        if parsed.loopback != wire.loopback {
            return Err(<D::Error as serde::de::Error>::custom(
                "endpoint loopback classification does not match its origin",
            ));
        }
        Ok(parsed)
    }
}

impl EndpointOrigin {
    pub fn parse(endpoint: &str, policy: EndpointPolicy) -> RuntimeAdapterResult<Self> {
        let parsed =
            Url::parse(endpoint).map_err(|error| RuntimeAdapterError::InvalidEndpoint {
                endpoint: endpoint.to_string(),
                message: error.to_string(),
            })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(invalid_endpoint(
                endpoint,
                "only http and https origins are supported",
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(invalid_endpoint(
                endpoint,
                "credentials are forbidden in runtime origins",
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(invalid_endpoint(
                endpoint,
                "query strings and fragments are forbidden",
            ));
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(invalid_endpoint(
                endpoint,
                "an endpoint must be an origin without a path",
            ));
        }
        let host = parsed
            .host()
            .ok_or_else(|| invalid_endpoint(endpoint, "missing host"))?;
        let loopback = match host {
            Host::Domain(domain) => {
                domain.eq_ignore_ascii_case("localhost")
                    || domain.to_ascii_lowercase().ends_with(".localhost")
            }
            Host::Ipv4(address) => address.is_loopback(),
            Host::Ipv6(address) => address.is_loopback(),
        };
        match policy {
            EndpointPolicy::LoopbackOnly if !loopback => {
                return Err(invalid_endpoint(
                    endpoint,
                    "only loopback origins are allowed",
                ));
            }
            EndpointPolicy::AllowRemoteHttps if !loopback && parsed.scheme() != "https" => {
                return Err(invalid_endpoint(
                    endpoint,
                    "remote runtime origins require https",
                ));
            }
            _ => {}
        }
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| invalid_endpoint(endpoint, "missing port for unknown scheme"))?;
        let host_text = match host {
            Host::Ipv6(address) => format!("[{address}]"),
            Host::Ipv4(address) => address.to_string(),
            Host::Domain(domain) => domain.to_ascii_lowercase(),
        };
        let default_port = (parsed.scheme() == "http" && port == 80)
            || (parsed.scheme() == "https" && port == 443);
        let origin = if default_port {
            format!("{}://{}", parsed.scheme(), host_text)
        } else {
            format!("{}://{}:{port}", parsed.scheme(), host_text)
        };
        Ok(Self { origin, loopback })
    }

    pub fn as_str(&self) -> &str {
        &self.origin
    }

    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    pub fn port(&self) -> u16 {
        Url::parse(&self.origin)
            .ok()
            .and_then(|url| url.port_or_known_default())
            .unwrap_or(0)
    }

    pub fn url(&self, absolute_path: &str) -> RuntimeAdapterResult<String> {
        if !absolute_path.starts_with('/')
            || absolute_path.contains("..")
            || absolute_path.contains('?')
            || absolute_path.contains('#')
            || absolute_path.contains('\0')
        {
            return Err(invalid_endpoint(
                absolute_path,
                "runtime API paths must be fixed absolute paths",
            ));
        }
        Ok(format!("{}{}", self.origin, absolute_path))
    }
}

fn invalid_endpoint(endpoint: &str, message: &str) -> RuntimeAdapterError {
    RuntimeAdapterError::InvalidEndpoint {
        endpoint: endpoint.to_string(),
        message: message.to_string(),
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub content_type: Option<String>,
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u64,
    pub max_response_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub trait HttpTransport: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a CancellationToken,
    ) -> RuntimeFuture<'a, HttpResponse>;
}

#[derive(Clone, Debug)]
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl ReqwestHttpTransport {
    pub fn new() -> RuntimeAdapterResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| RuntimeAdapterError::Transport {
                operation: "build HTTP client".to_string(),
                message: error.to_string(),
            })?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestHttpTransport {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a CancellationToken,
    ) -> RuntimeFuture<'a, HttpResponse> {
        Box::pin(async move {
            if request.max_response_bytes == 0
                || request.max_response_bytes > ABSOLUTE_MAX_RESPONSE_BYTES
                || request.timeout_ms == 0
                || request.timeout_ms > ABSOLUTE_MAX_TIMEOUT_MS
            {
                return Err(RuntimeAdapterError::InvalidOperationLimits {
                    message: "HTTP request limits exceed hard safety caps".to_string(),
                });
            }
            let method = match request.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
            };
            let mut builder = self.client.request(method, &request.url);
            if let Some(content_type) = request.content_type.as_deref() {
                builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let timeout_ms = request.timeout_ms;
            let max_response_bytes = request.max_response_bytes;
            let operation = async {
                let response = tokio::select! {
                    _ = cancellation.cancelled() => return Err(RuntimeAdapterError::Cancelled {
                        operation: "HTTP request".to_string(),
                    }),
                    response = crate::egress::send(builder) => response.map_err(|error| RuntimeAdapterError::Transport {
                        operation: "HTTP request".to_string(),
                        message: error.to_string(),
                    })?,
                };
                let status = response.status().as_u16();
                let mut stream = response.bytes_stream();
                let mut body = Vec::new();
                loop {
                    let next = tokio::select! {
                        _ = cancellation.cancelled() => return Err(RuntimeAdapterError::Cancelled {
                            operation: "HTTP response".to_string(),
                        }),
                        next = stream.next() => next,
                    };
                    let Some(chunk) = next else { break };
                    let chunk = chunk.map_err(|error| RuntimeAdapterError::Transport {
                        operation: "read HTTP response".to_string(),
                        message: error.to_string(),
                    })?;
                    let next_len = body.len().saturating_add(chunk.len());
                    if next_len > max_response_bytes {
                        return Err(RuntimeAdapterError::ResponseTooLarge {
                            limit: max_response_bytes,
                            actual_at_least: next_len,
                        });
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(HttpResponse { status, body })
            };
            tokio::time::timeout(Duration::from_millis(timeout_ms), operation)
                .await
                .map_err(|_| RuntimeAdapterError::Timeout {
                    operation: "HTTP request".to_string(),
                    timeout_ms,
                })?
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Ollama,
    LlamaCpp,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorKind {
    Cpu,
    Metal,
    Cuda,
    Rocm,
    Vulkan,
    DirectMl,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcceleratorCapability {
    pub kind: AcceleratorKind,
    pub available: bool,
    pub device_names: Vec<String>,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlatformCapabilities {
    pub os: String,
    pub arch: String,
    pub supported_runtimes: Vec<RuntimeKind>,
    pub accelerators: Vec<AcceleratorCapability>,
}

impl PlatformCapabilities {
    pub fn from_host(os: &str, arch: &str, detected: Vec<AcceleratorCapability>) -> Self {
        let os = normalize_os(os);
        let arch = normalize_arch(arch);
        let supported_host = matches!(os.as_str(), "macos" | "linux" | "windows")
            && matches!(arch.as_str(), "x86_64" | "aarch64");
        let supported_runtimes = if supported_host {
            vec![RuntimeKind::Ollama, RuntimeKind::LlamaCpp]
        } else {
            Vec::new()
        };
        let mut by_kind = BTreeMap::new();
        by_kind.insert(
            AcceleratorKind::Cpu,
            AcceleratorCapability {
                kind: AcceleratorKind::Cpu,
                available: supported_host,
                device_names: Vec::new(),
                total_memory_bytes: None,
                available_memory_bytes: None,
            },
        );
        for capability in detected {
            let permitted = match capability.kind {
                AcceleratorKind::Cpu => true,
                AcceleratorKind::Metal => os == "macos",
                AcceleratorKind::Cuda => matches!(os.as_str(), "linux" | "windows"),
                AcceleratorKind::Rocm => matches!(os.as_str(), "linux" | "windows"),
                AcceleratorKind::Vulkan => matches!(os.as_str(), "linux" | "windows"),
                AcceleratorKind::DirectMl => os == "windows",
            };
            if permitted {
                by_kind.insert(capability.kind, capability);
            }
        }
        Self {
            os,
            arch,
            supported_runtimes,
            accelerators: by_kind.into_values().collect(),
        }
    }

    pub fn current(detected: Vec<AcceleratorCapability>) -> Self {
        Self::from_host(std::env::consts::OS, std::env::consts::ARCH, detected)
    }

    pub fn supports_runtime(&self, runtime: RuntimeKind) -> bool {
        self.supported_runtimes.contains(&runtime)
    }

    pub fn supports_accelerator(&self, accelerator: AcceleratorKind) -> bool {
        self.accelerators
            .iter()
            .any(|entry| entry.kind == accelerator && entry.available)
    }
}

fn normalize_os(os: &str) -> String {
    match os.trim().to_ascii_lowercase().as_str() {
        "darwin" | "mac" | "macos" => "macos".to_string(),
        "win" | "win32" | "windows" => "windows".to_string(),
        "linux" => "linux".to_string(),
        other => other.to_string(),
    }
}

fn normalize_arch(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "aarch64".to_string(),
        "amd64" | "x64" | "x86_64" => "x86_64".to_string(),
        other => other.to_string(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HardwareSnapshot {
    pub captured_at_ms: u64,
    pub total_ram_bytes: u64,
    pub available_ram_bytes: u64,
    pub logical_cpu_count: u32,
    pub platform: PlatformCapabilities,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HardwareTier {
    Constrained,
    Balanced,
    Performance,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HardwareProfile {
    pub tier: HardwareTier,
    pub recommended_process_slots: u16,
    pub recommended_ram_reserve_bytes: u64,
    pub preferred_accelerator: AcceleratorKind,
}

impl HardwareSnapshot {
    pub fn profile(&self) -> RuntimeAdapterResult<HardwareProfile> {
        if self.total_ram_bytes == 0
            || self.available_ram_bytes > self.total_ram_bytes
            || self.logical_cpu_count == 0
        {
            return Err(RuntimeAdapterError::InvalidOperationLimits {
                message: "hardware snapshot contains impossible memory or CPU values".to_string(),
            });
        }
        const GIB: u64 = 1024 * 1024 * 1024;
        let (tier, slots, reserve) = if self.total_ram_bytes < 12 * GIB {
            (HardwareTier::Constrained, 1, 2 * GIB)
        } else if self.total_ram_bytes < 32 * GIB {
            (HardwareTier::Balanced, 2, 3 * GIB)
        } else {
            (HardwareTier::Performance, 4, 4 * GIB)
        };
        let cpu_bound_slots =
            u16::try_from((self.logical_cpu_count / 2).max(1)).unwrap_or(u16::MAX);
        let preferred_accelerator = [
            AcceleratorKind::Metal,
            AcceleratorKind::Cuda,
            AcceleratorKind::Rocm,
            AcceleratorKind::DirectMl,
            AcceleratorKind::Vulkan,
            AcceleratorKind::Cpu,
        ]
        .into_iter()
        .find(|kind| self.platform.supports_accelerator(*kind))
        .unwrap_or(AcceleratorKind::Cpu);
        Ok(HardwareProfile {
            tier,
            recommended_process_slots: slots.min(cpu_bound_slots),
            recommended_ram_reserve_bytes: reserve.min(self.total_ram_bytes / 2),
            preferred_accelerator,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDescriptor {
    pub schema_version: u32,
    pub runtime_id: String,
    pub kind: RuntimeKind,
    pub label: String,
    pub endpoint: EndpointOrigin,
    pub managed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingValue {
    Boolean { value: bool },
    Integer { value: i64 },
    Float { value: f64 },
    Text { value: String },
    Choice { value: String },
    DurationMs { value: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingValueSchema {
    Boolean,
    Integer { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64, step: f64 },
    Text { max_bytes: usize },
    Choice { options: Vec<String> },
    DurationMs { min: u64, max: u64, step: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdvancedSettingCapability {
    pub key: String,
    pub label: String,
    pub description: String,
    pub schema: SettingValueSchema,
    pub default_value: SettingValue,
    pub restart_required: bool,
    /// Whether this control can actually be enabled right now. Every
    /// capability declared here is one the runtime driver knows how to
    /// accept in principle; `supported` narrows that down to what the
    /// *current* runtime/model/hardware combination can honor. A freshly
    /// constructed adapter's baseline `capabilities()` has no hardware or
    /// selected-model context, so it always reports `true` here except for
    /// controls (like the speculative-decoding draft model) that are
    /// inherently model-relative and therefore unknown until a model is
    /// selected. The Runtime Hub layer (`m3_runtime_hub.rs`'s
    /// `gate_advanced_settings`) narrows this further using the Hardware
    /// Compatibility report and the installed-model catalog before the UI
    /// ever renders a control — see that function's doc comment for exactly
    /// which keys it gates and why. This is advisory for the UI only: the
    /// hub also enforces the same gates server-side (`set_runtime_config`/
    /// `load_model`) so a control can never actually take effect just
    /// because a client skipped the UI and submitted a value directly.
    pub supported: bool,
    /// Present exactly when `supported` is `false`: a short, human-readable
    /// reason to surface directly next to the disabled control. Never leave
    /// a control disabled with no explanation.
    pub unsupported_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapabilities {
    pub can_start: bool,
    pub can_stop: bool,
    pub can_inventory: bool,
    pub can_load: bool,
    pub can_unload: bool,
    pub can_set_keep_alive: bool,
    pub can_tail_logs: bool,
    pub platform: PlatformCapabilities,
    pub settings: Vec<AdvancedSettingCapability>,
}

pub fn validate_setting_values(
    runtime_id: &str,
    capabilities: &[AdvancedSettingCapability],
    values: &BTreeMap<String, SettingValue>,
    max_config_bytes: usize,
) -> RuntimeAdapterResult<()> {
    if values.len() > MAX_SETTINGS {
        return Err(RuntimeAdapterError::InvalidSetting {
            key: "<settings>".to_string(),
            message: format!("at most {MAX_SETTINGS} settings are accepted"),
        });
    }
    for (key, value) in values {
        validate_setting_key(key)?;
        let capability = capabilities
            .iter()
            .find(|capability| capability.key == *key)
            .ok_or_else(|| RuntimeAdapterError::UnsupportedSetting {
                runtime_id: runtime_id.to_string(),
                key: key.clone(),
            })?;
        validate_setting_value(key, &capability.schema, value)?;
    }
    let encoded =
        serde_json::to_vec(values).map_err(|error| RuntimeAdapterError::InvalidSetting {
            key: "<settings>".to_string(),
            message: error.to_string(),
        })?;
    if encoded.len() > max_config_bytes {
        return Err(RuntimeAdapterError::ConfigTooLarge {
            limit: max_config_bytes,
            actual: encoded.len(),
        });
    }
    Ok(())
}

fn validate_setting_value(
    key: &str,
    schema: &SettingValueSchema,
    value: &SettingValue,
) -> RuntimeAdapterResult<()> {
    let valid = match (schema, value) {
        (SettingValueSchema::Boolean, SettingValue::Boolean { .. }) => true,
        (SettingValueSchema::Integer { min, max, step }, SettingValue::Integer { value }) => {
            *step > 0 && value >= min && value <= max && (value - min) % step == 0
        }
        (SettingValueSchema::Float { min, max, step }, SettingValue::Float { value }) => {
            value.is_finite()
                && min.is_finite()
                && max.is_finite()
                && step.is_finite()
                && *step > 0.0
                && value >= min
                && value <= max
        }
        (SettingValueSchema::Text { max_bytes }, SettingValue::Text { value }) => {
            value.len() <= (*max_bytes).min(MAX_SETTING_STRING_BYTES) && !value.contains('\0')
        }
        (SettingValueSchema::Choice { options }, SettingValue::Choice { value }) => {
            !options.is_empty() && options.contains(value)
        }
        (SettingValueSchema::DurationMs { min, max, step }, SettingValue::DurationMs { value }) => {
            *step > 0 && value >= min && value <= max && (value - min) % step == 0
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "value does not satisfy the advertised capability schema".to_string(),
        })
    }
}

fn validate_setting_key(key: &str) -> RuntimeAdapterResult<()> {
    let valid = !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "setting keys must be lowercase ASCII identifiers".to_string(),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLifecycleState {
    Stopped,
    Starting,
    Ready,
    Degraded,
    Unreachable,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStatus {
    pub runtime: RuntimeDescriptor,
    pub state: RuntimeLifecycleState,
    pub version: Option<String>,
    pub process: Option<ManagedProcessHandle>,
    pub message: Option<String>,
    pub checked_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub chat: bool,
    pub embeddings: bool,
    pub tool_calling: bool,
    pub vision: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeModel {
    pub model_id: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub local_path: Option<PathBuf>,
    pub digest: Option<String>,
    pub modified_at: Option<String>,
    pub capabilities: ModelCapabilities,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInventory {
    pub schema_version: u32,
    pub runtime_id: String,
    pub models: Vec<RuntimeModel>,
    pub captured_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ResidencyOwnership {
    PreExisting,
    AppManaged,
    External,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunningModel {
    pub runtime_id: String,
    pub model_id: String,
    pub size_bytes: u64,
    pub memory_bytes: u64,
    pub vram_bytes: u64,
    pub digest: Option<String>,
    pub expires_at: Option<String>,
    pub ownership: ResidencyOwnership,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelLoadRequest {
    pub model_id: String,
    pub keep_alive: Option<KeepAlive>,
    pub settings: BTreeMap<String, SettingValue>,
    pub replace_existing: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStartRequest {
    pub initial_model: Option<ModelLoadRequest>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum KeepAlive {
    DurationMs { milliseconds: u64 },
    Forever,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelLoadDisposition {
    Loaded,
    AlreadyResident,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelLoadOutcome {
    pub runtime_id: String,
    pub model_id: String,
    pub disposition: ModelLoadDisposition,
    pub ownership: ResidencyOwnership,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnloadPolicy {
    AppManagedOnly,
    ExactRegardlessOfOwner,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelUnloadRequest {
    pub model_id: String,
    pub policy: UnloadPolicy,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnloadDisposition {
    Unloaded,
    NotRunning,
    PreservedPreExisting,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelUnloadOutcome {
    pub runtime_id: String,
    pub model_id: String,
    pub disposition: ModelUnloadDisposition,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeepAliveRequest {
    pub model_id: String,
    pub keep_alive: KeepAlive,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLogRequest {
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLogTail {
    pub text: String,
    pub truncated: bool,
}

pub trait RuntimeAdapter: Send + Sync {
    fn descriptor(&self) -> RuntimeDescriptor;
    fn capabilities(&self) -> RuntimeCapabilities;
    fn status<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus>;
    fn inventory<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeInventory>;
    fn running_models<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Vec<RunningModel>>;
    fn start<'a>(
        &'a self,
        request: &'a RuntimeStartRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus>;
    fn stop<'a>(&'a self, context: &'a RuntimeOperationContext)
        -> RuntimeFuture<'a, RuntimeStatus>;
    fn load_model<'a>(
        &'a self,
        request: &'a ModelLoadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelLoadOutcome>;
    fn unload_model<'a>(
        &'a self,
        request: &'a ModelUnloadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelUnloadOutcome>;
    fn set_keep_alive<'a>(
        &'a self,
        request: &'a KeepAliveRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()>;
    fn tail_logs<'a>(
        &'a self,
        request: &'a RuntimeLogRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeLogTail>;
}

#[derive(Debug, Deserialize)]
struct OllamaVersionResponse {
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    modified_at: String,
}

#[derive(Debug, Deserialize)]
struct OllamaRunningResponse {
    #[serde(default)]
    models: Vec<OllamaRunningEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaRunningEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    size_vram: u64,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    expires_at: String,
}

pub struct OllamaHttpAdapter {
    descriptor: RuntimeDescriptor,
    capabilities: RuntimeCapabilities,
    transport: Arc<dyn HttpTransport>,
    owned_residency: Mutex<BTreeSet<String>>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl OllamaHttpAdapter {
    pub fn new(
        runtime_id: impl Into<String>,
        endpoint: &str,
        endpoint_policy: EndpointPolicy,
        transport: Arc<dyn HttpTransport>,
        platform: PlatformCapabilities,
    ) -> RuntimeAdapterResult<Self> {
        let runtime_id = runtime_id.into();
        validate_runtime_id(&runtime_id)?;
        let endpoint = EndpointOrigin::parse(endpoint, endpoint_policy)?;
        let capabilities = RuntimeCapabilities {
            can_start: false,
            can_stop: false,
            can_inventory: true,
            can_load: true,
            can_unload: true,
            can_set_keep_alive: true,
            can_tail_logs: false,
            platform,
            settings: ollama_setting_capabilities(),
        };
        Ok(Self {
            descriptor: RuntimeDescriptor {
                schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id,
                kind: RuntimeKind::Ollama,
                label: "Ollama".to_string(),
                endpoint,
                managed: false,
            },
            capabilities,
            transport,
            owned_residency: Mutex::new(BTreeSet::new()),
            mutation_lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn request(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
        operation: &str,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<HttpResponse> {
        context.preflight(operation)?;
        let encoded = match body {
            Some(body) => {
                let encoded = serde_json::to_vec(&body).map_err(|error| {
                    RuntimeAdapterError::InvalidSetting {
                        key: "<request>".to_string(),
                        message: error.to_string(),
                    }
                })?;
                if encoded.len() > context.limits.max_config_bytes {
                    return Err(RuntimeAdapterError::ConfigTooLarge {
                        limit: context.limits.max_config_bytes,
                        actual: encoded.len(),
                    });
                }
                Some(encoded)
            }
            None => None,
        };
        let request = HttpRequest {
            method,
            url: self.descriptor.endpoint.url(path)?,
            content_type: encoded.as_ref().map(|_| "application/json".to_string()),
            body: encoded,
            timeout_ms: context.limits.timeout_ms,
            max_response_bytes: context.limits.max_response_bytes,
        };
        let response = bounded_operation(
            context,
            operation,
            self.transport.execute(request, &context.cancellation),
        )
        .await?;
        if response.body.len() > context.limits.max_response_bytes {
            return Err(RuntimeAdapterError::ResponseTooLarge {
                limit: context.limits.max_response_bytes,
                actual_at_least: response.body.len(),
            });
        }
        if !(200..300).contains(&response.status) {
            return Err(RuntimeAdapterError::HttpStatus {
                operation: operation.to_string(),
                status: response.status,
                body: String::from_utf8_lossy(&response.body).trim().to_string(),
            });
        }
        Ok(response)
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
        operation: &str,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<T> {
        let response = self.request(method, path, body, operation, context).await?;
        serde_json::from_slice(&response.body).map_err(|error| {
            RuntimeAdapterError::MalformedResponse {
                operation: operation.to_string(),
                message: error.to_string(),
            }
        })
    }

    async fn running_models_impl(
        &self,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<Vec<RunningModel>> {
        let parsed: OllamaRunningResponse = self
            .request_json(
                HttpMethod::Get,
                "/api/ps",
                None,
                "list running Ollama models",
                context,
            )
            .await?;
        if parsed.models.len() > MAX_MODELS_PER_RESPONSE {
            return Err(RuntimeAdapterError::MalformedResponse {
                operation: "list running Ollama models".to_string(),
                message: format!("response contains more than {MAX_MODELS_PER_RESPONSE} models"),
            });
        }
        let owned = lock(&self.owned_residency)?.clone();
        parsed
            .models
            .into_iter()
            .filter_map(|entry| {
                let OllamaRunningEntry {
                    name,
                    model,
                    size,
                    size_vram,
                    digest,
                    expires_at,
                } = entry;
                let model_id = name.or(model)?;
                Some((model_id, size, size_vram, digest, expires_at))
            })
            .map(|(model_id, size, size_vram, digest, expires_at)| {
                validate_model_id(&model_id).map(|()| RunningModel {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    ownership: if owned.contains(&model_id) {
                        ResidencyOwnership::AppManaged
                    } else {
                        ResidencyOwnership::PreExisting
                    },
                    model_id,
                    size_bytes: size,
                    memory_bytes: size,
                    vram_bytes: size_vram,
                    digest: nonempty(digest),
                    expires_at: nonempty(expires_at),
                })
            })
            .collect()
    }

    async fn post_keep_alive(
        &self,
        model_id: &str,
        keep_alive: Value,
        settings: &BTreeMap<String, SettingValue>,
        operation: &str,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<()> {
        let options = ollama_settings_json(settings)?;
        let mut body = Map::new();
        body.insert("model".to_string(), Value::String(model_id.to_string()));
        body.insert("messages".to_string(), Value::Array(Vec::new()));
        body.insert("keep_alive".to_string(), keep_alive);
        body.insert("stream".to_string(), Value::Bool(false));
        if !options.is_empty() {
            body.insert("options".to_string(), Value::Object(options));
        }
        self.request(
            HttpMethod::Post,
            "/api/chat",
            Some(Value::Object(body)),
            operation,
            context,
        )
        .await?;
        Ok(())
    }

    fn unsupported(&self, capability: &str) -> RuntimeAdapterError {
        RuntimeAdapterError::UnsupportedCapability {
            runtime_id: self.descriptor.runtime_id.clone(),
            capability: capability.to_string(),
        }
    }
}

impl RuntimeAdapter for OllamaHttpAdapter {
    fn descriptor(&self) -> RuntimeDescriptor {
        self.descriptor.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        self.capabilities.clone()
    }

    fn status<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move {
            let parsed: OllamaVersionResponse = self
                .request_json(
                    HttpMethod::Get,
                    "/api/version",
                    None,
                    "query Ollama status",
                    context,
                )
                .await?;
            Ok(RuntimeStatus {
                runtime: self.descriptor.clone(),
                state: RuntimeLifecycleState::Ready,
                version: parsed.version,
                process: None,
                message: None,
                checked_at_ms: now_ms(),
            })
        })
    }

    fn inventory<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            let parsed: OllamaTagsResponse = self
                .request_json(
                    HttpMethod::Get,
                    "/api/tags",
                    None,
                    "list Ollama model inventory",
                    context,
                )
                .await?;
            if parsed.models.len() > MAX_MODELS_PER_RESPONSE {
                return Err(RuntimeAdapterError::MalformedResponse {
                    operation: "list Ollama model inventory".to_string(),
                    message: format!(
                        "response contains more than {MAX_MODELS_PER_RESPONSE} models"
                    ),
                });
            }
            let mut models = Vec::new();
            for entry in parsed.models {
                let Some(model_id) = entry.name.or(entry.model) else {
                    continue;
                };
                validate_model_id(&model_id)?;
                let mut metadata = BTreeMap::new();
                metadata.insert(
                    "is_cloud".to_string(),
                    model_id.to_ascii_lowercase().contains("cloud").to_string(),
                );
                models.push(RuntimeModel {
                    display_name: model_id.clone(),
                    model_id,
                    size_bytes: entry.size,
                    local_path: None,
                    digest: nonempty(entry.digest),
                    modified_at: nonempty(entry.modified_at),
                    capabilities: ModelCapabilities {
                        chat: true,
                        embeddings: false,
                        tool_calling: false,
                        vision: false,
                    },
                    metadata,
                });
            }
            models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
            Ok(RuntimeInventory {
                schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id: self.descriptor.runtime_id.clone(),
                models,
                captured_at_ms: now_ms(),
            })
        })
    }

    fn running_models<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Vec<RunningModel>> {
        Box::pin(async move { self.running_models_impl(context).await })
    }

    fn start<'a>(
        &'a self,
        _request: &'a RuntimeStartRequest,
        _context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move { Err(self.unsupported("managed start")) })
    }

    fn stop<'a>(
        &'a self,
        _context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move { Err(self.unsupported("managed stop")) })
    }

    fn load_model<'a>(
        &'a self,
        request: &'a ModelLoadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelLoadOutcome> {
        Box::pin(async move {
            context.preflight("load Ollama model")?;
            let _mutation_guard =
                async_lock(&self.mutation_lock, context, "queue Ollama model load").await?;
            validate_model_id(&request.model_id)?;
            validate_setting_values(
                &self.descriptor.runtime_id,
                &self.capabilities.settings,
                &request.settings,
                context.limits.max_config_bytes,
            )?;
            let keep_alive =
                keep_alive_json(request.keep_alive.unwrap_or(KeepAlive::DurationMs {
                    milliseconds: 5 * 60 * 1_000,
                }))?;
            let before = self.running_models_impl(context).await?;
            let existing = before
                .iter()
                .find(|entry| entry.model_id == request.model_id);
            let disposition = if existing.is_some() {
                ModelLoadDisposition::AlreadyResident
            } else {
                if lock(&self.owned_residency)?.len() >= MAX_MODELS_PER_RESPONSE {
                    return Err(RuntimeAdapterError::ConfigTooLarge {
                        limit: MAX_MODELS_PER_RESPONSE,
                        actual: MAX_MODELS_PER_RESPONSE + 1,
                    });
                }
                ModelLoadDisposition::Loaded
            };
            self.post_keep_alive(
                &request.model_id,
                keep_alive,
                &request.settings,
                "load Ollama model",
                context,
            )
            .await?;
            let ownership = if let Some(existing) = existing {
                existing.ownership
            } else {
                lock(&self.owned_residency)?.insert(request.model_id.clone());
                ResidencyOwnership::AppManaged
            };
            Ok(ModelLoadOutcome {
                runtime_id: self.descriptor.runtime_id.clone(),
                model_id: request.model_id.clone(),
                disposition,
                ownership,
            })
        })
    }

    fn unload_model<'a>(
        &'a self,
        request: &'a ModelUnloadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelUnloadOutcome> {
        Box::pin(async move {
            context.preflight("unload Ollama model")?;
            let _mutation_guard =
                async_lock(&self.mutation_lock, context, "queue Ollama model unload").await?;
            validate_model_id(&request.model_id)?;
            let running = self.running_models_impl(context).await?;
            let exact = running
                .iter()
                .find(|entry| entry.model_id == request.model_id);
            let Some(exact) = exact else {
                return Ok(ModelUnloadOutcome {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                    disposition: ModelUnloadDisposition::NotRunning,
                });
            };
            if request.policy == UnloadPolicy::AppManagedOnly
                && exact.ownership != ResidencyOwnership::AppManaged
            {
                return Ok(ModelUnloadOutcome {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                    disposition: ModelUnloadDisposition::PreservedPreExisting,
                });
            }
            self.post_keep_alive(
                &request.model_id,
                Value::Number(0.into()),
                &BTreeMap::new(),
                "unload exact Ollama model",
                context,
            )
            .await?;
            lock(&self.owned_residency)?.remove(&request.model_id);
            Ok(ModelUnloadOutcome {
                runtime_id: self.descriptor.runtime_id.clone(),
                model_id: request.model_id.clone(),
                disposition: ModelUnloadDisposition::Unloaded,
            })
        })
    }

    fn set_keep_alive<'a>(
        &'a self,
        request: &'a KeepAliveRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            context.preflight("set Ollama keep-alive")?;
            let _mutation_guard = async_lock(
                &self.mutation_lock,
                context,
                "queue Ollama keep-alive update",
            )
            .await?;
            validate_model_id(&request.model_id)?;
            let running = self.running_models_impl(context).await?;
            if !running
                .iter()
                .any(|entry| entry.model_id == request.model_id)
            {
                return Err(RuntimeAdapterError::ModelNotRunning {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                });
            }
            self.post_keep_alive(
                &request.model_id,
                keep_alive_json(request.keep_alive)?,
                &BTreeMap::new(),
                "set Ollama keep-alive",
                context,
            )
            .await
        })
    }

    fn tail_logs<'a>(
        &'a self,
        _request: &'a RuntimeLogRequest,
        _context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeLogTail> {
        Box::pin(async move { Err(self.unsupported("log tail")) })
    }
}

fn ollama_setting_capabilities() -> Vec<AdvancedSettingCapability> {
    vec![
        AdvancedSettingCapability {
            key: "num_ctx".to_string(),
            label: "Context size".to_string(),
            description: "Ollama context window used while loading the model.".to_string(),
            schema: SettingValueSchema::Integer {
                min: 128,
                max: 1_048_576,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 4_096 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "num_gpu".to_string(),
            label: "GPU layers".to_string(),
            description: "Number of model layers assigned to an accelerator.".to_string(),
            schema: SettingValueSchema::Integer {
                min: -1,
                max: 999,
                step: 1,
            },
            default_value: SettingValue::Integer { value: -1 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "use_mmap".to_string(),
            label: "Memory map".to_string(),
            description: "Allow Ollama to memory-map model files.".to_string(),
            schema: SettingValueSchema::Boolean,
            default_value: SettingValue::Boolean { value: true },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "use_mlock".to_string(),
            label: "Lock memory".to_string(),
            description: "Ask Ollama to keep model pages resident in memory.".to_string(),
            schema: SettingValueSchema::Boolean,
            default_value: SettingValue::Boolean { value: false },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        // -- Sampler and batching controls (ROADMAP Phase 8 item 17) --
        // Ollama forwards its `options` object straight through to the
        // embedded llama.cpp engine (see `ollama_settings_json`, which
        // already serializes any key/value pair generically), so these need
        // no new wire-format work — only the capability declaration below.
        // None of these depend on hardware or the selected model: Ollama
        // accepts them for every model it can load, so they are never
        // gated (`supported: true` unconditionally).
        AdvancedSettingCapability {
            key: "temperature".to_string(),
            label: "Temperature".to_string(),
            description: "Sampling temperature: higher values increase randomness.".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 2.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.8 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "top_p".to_string(),
            label: "Top-p".to_string(),
            description: "Nucleus sampling probability mass cutoff.".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 1.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.9 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "top_k".to_string(),
            label: "Top-k".to_string(),
            description: "Limits sampling to the k most likely next tokens.".to_string(),
            schema: SettingValueSchema::Integer {
                min: 0,
                max: 1_000,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 40 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "repeat_penalty".to_string(),
            label: "Repeat penalty".to_string(),
            description: "Penalizes tokens that already appeared in the context.".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 2.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 1.1 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "min_p".to_string(),
            label: "Min-p".to_string(),
            description: "Minimum token probability, relative to the most likely token."
                .to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 1.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.05 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "num_batch".to_string(),
            label: "Batch size".to_string(),
            description: "Number of tokens Ollama processes together per batch.".to_string(),
            schema: SettingValueSchema::Integer {
                min: 1,
                max: 8_192,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 512 },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        },
    ]
}

fn ollama_settings_json(
    settings: &BTreeMap<String, SettingValue>,
) -> RuntimeAdapterResult<Map<String, Value>> {
    let mut object = Map::new();
    for (key, value) in settings {
        let value = match value {
            SettingValue::Boolean { value } => Value::Bool(*value),
            SettingValue::Integer { value } => Value::Number((*value).into()),
            SettingValue::Float { value } => serde_json::Number::from_f64(*value)
                .map(Value::Number)
                .ok_or_else(|| RuntimeAdapterError::InvalidSetting {
                    key: key.clone(),
                    message: "float must be finite".to_string(),
                })?,
            SettingValue::Text { value } | SettingValue::Choice { value } => {
                Value::String(value.clone())
            }
            SettingValue::DurationMs { value } => Value::Number((*value).into()),
        };
        object.insert(key.clone(), value);
    }
    Ok(object)
}

fn keep_alive_json(keep_alive: KeepAlive) -> RuntimeAdapterResult<Value> {
    match keep_alive {
        KeepAlive::DurationMs { milliseconds } if milliseconds > 0 => {
            Ok(Value::String(format!("{milliseconds}ms")))
        }
        KeepAlive::DurationMs { .. } => Err(RuntimeAdapterError::InvalidSetting {
            key: "keep_alive".to_string(),
            message: "duration must be greater than zero; use unload for zero".to_string(),
        }),
        KeepAlive::Forever => Ok(Value::String("-1".to_string())),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortOwnership {
    pub port: u16,
    pub owner_id: String,
    pub runtime: Option<RuntimeKind>,
    pub ownership: ResidencyOwnership,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedProcessHandle {
    pub process_id: String,
    pub os_pid: Option<u32>,
    pub port: u16,
    pub started_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedProcessState {
    Starting,
    Ready,
    Exited,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedProcessStatus {
    pub handle: ManagedProcessHandle,
    pub state: ManagedProcessState,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedProcessSpec {
    pub runtime_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub port: u16,
}

impl ManagedProcessSpec {
    pub fn validate(&self, max_config_bytes: usize) -> RuntimeAdapterResult<()> {
        validate_runtime_id(&self.runtime_id)?;
        if self.port == 0 {
            return Err(RuntimeAdapterError::InvalidProcessSpec {
                message: "port zero is not a valid listening port".to_string(),
            });
        }
        if !self.program.is_absolute() || self.program.as_os_str().is_empty() {
            return Err(RuntimeAdapterError::InvalidProcessSpec {
                message: "the executable must be an absolute path".to_string(),
            });
        }
        if self.args.len() > 128
            || self.args.iter().any(|argument| {
                argument.len() > MAX_SETTING_STRING_BYTES || argument.contains('\0')
            })
        {
            return Err(RuntimeAdapterError::InvalidProcessSpec {
                message: "argument count or size exceeds the structured process limit".to_string(),
            });
        }
        let encoded =
            serde_json::to_vec(self).map_err(|error| RuntimeAdapterError::InvalidProcessSpec {
                message: error.to_string(),
            })?;
        if encoded.len() > max_config_bytes {
            return Err(RuntimeAdapterError::ConfigTooLarge {
                limit: max_config_bytes,
                actual: encoded.len(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedLogChunk {
    pub text: String,
    pub truncated: bool,
}

/// Controller contract for a managed runtime process.
///
/// `launch` receives an executable plus an argument vector. Implementations
/// must pass them to a process API one argument at a time; the contract has no
/// shell-string field. `launch` should return only after the runtime is ready
/// or a bounded readiness check has failed.
pub trait ManagedProcessController: Send + Sync {
    fn port_owner<'a>(
        &'a self,
        port: u16,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Option<PortOwnership>>;
    fn launch<'a>(
        &'a self,
        spec: ManagedProcessSpec,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessHandle>;
    fn inspect<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessStatus>;
    fn terminate<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()>;
    fn tail_logs<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        max_bytes: usize,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedLogChunk>;
}

#[derive(Clone, Debug)]
struct ManagedLlamaResidency {
    model: RuntimeModel,
    handle: ManagedProcessHandle,
}

pub struct ManagedLlamaCppAdapter {
    descriptor: RuntimeDescriptor,
    capabilities: RuntimeCapabilities,
    executable: PathBuf,
    port: u16,
    controller: Arc<dyn ManagedProcessController>,
    models: Vec<RuntimeModel>,
    residency: Mutex<Option<ManagedLlamaResidency>>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl ManagedLlamaCppAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime_id: impl Into<String>,
        endpoint: &str,
        executable: PathBuf,
        port: u16,
        controller: Arc<dyn ManagedProcessController>,
        mut models: Vec<RuntimeModel>,
        platform: PlatformCapabilities,
    ) -> RuntimeAdapterResult<Self> {
        let runtime_id = runtime_id.into();
        validate_runtime_id(&runtime_id)?;
        let endpoint = EndpointOrigin::parse(endpoint, EndpointPolicy::LoopbackOnly)?;
        if port == 0 || endpoint.port() != port {
            return Err(RuntimeAdapterError::InvalidEndpoint {
                endpoint: endpoint.as_str().to_string(),
                message: format!("origin port must exactly match managed port {port}"),
            });
        }
        if !executable.is_absolute() || executable.as_os_str().is_empty() {
            return Err(RuntimeAdapterError::InvalidProcessSpec {
                message: "llama-server executable must be an absolute path".to_string(),
            });
        }
        if models.len() > MAX_MODELS_PER_RESPONSE {
            return Err(RuntimeAdapterError::ConfigTooLarge {
                limit: MAX_MODELS_PER_RESPONSE,
                actual: models.len(),
            });
        }
        let mut ids = BTreeSet::new();
        for model in &models {
            validate_model_id(&model.model_id)?;
            if !ids.insert(model.model_id.clone()) {
                return Err(RuntimeAdapterError::InvalidIdentifier {
                    field: "model_id",
                    value: model.model_id.clone(),
                });
            }
            if let Some(path) = model.local_path.as_ref() {
                if !path.is_absolute() {
                    return Err(RuntimeAdapterError::ModelPathUnavailable {
                        runtime_id: runtime_id.clone(),
                        model_id: model.model_id.clone(),
                    });
                }
            }
        }
        let encoded = serde_json::to_vec(&models).map_err(|error| {
            RuntimeAdapterError::InvalidProcessSpec {
                message: error.to_string(),
            }
        })?;
        if encoded.len() > ABSOLUTE_MAX_CONFIG_BYTES {
            return Err(RuntimeAdapterError::ConfigTooLarge {
                limit: ABSOLUTE_MAX_CONFIG_BYTES,
                actual: encoded.len(),
            });
        }
        models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
        Ok(Self {
            descriptor: RuntimeDescriptor {
                schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id,
                kind: RuntimeKind::LlamaCpp,
                label: "llama.cpp".to_string(),
                endpoint,
                managed: true,
            },
            capabilities: RuntimeCapabilities {
                can_start: true,
                can_stop: true,
                can_inventory: true,
                can_load: true,
                can_unload: true,
                can_set_keep_alive: false,
                can_tail_logs: true,
                platform,
                settings: llama_setting_capabilities(),
            },
            executable,
            port,
            controller,
            models,
            residency: Mutex::new(None),
            mutation_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn unsupported(&self, capability: &str) -> RuntimeAdapterError {
        RuntimeAdapterError::UnsupportedCapability {
            runtime_id: self.descriptor.runtime_id.clone(),
            capability: capability.to_string(),
        }
    }

    fn current_residency(&self) -> RuntimeAdapterResult<Option<ManagedLlamaResidency>> {
        Ok(lock(&self.residency)?.clone())
    }

    fn clear_residency_if(&self, process_id: &str) -> RuntimeAdapterResult<()> {
        let mut residency = lock(&self.residency)?;
        if residency
            .as_ref()
            .is_some_and(|entry| entry.handle.process_id == process_id)
        {
            *residency = None;
        }
        Ok(())
    }

    async fn inspect_residency(
        &self,
        residency: &ManagedLlamaResidency,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<ManagedProcessStatus> {
        let status = bounded_operation(
            context,
            "inspect managed llama.cpp process",
            self.controller.inspect(&residency.handle, context),
        )
        .await?;
        ensure_serialized_response_size(
            &status,
            context.limits.max_response_bytes,
            "inspect managed llama.cpp process",
        )?;
        Ok(status)
    }

    async fn stop_current(
        &self,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<Option<ManagedLlamaResidency>> {
        let current = self.current_residency()?;
        if let Some(current) = current.as_ref() {
            bounded_operation(
                context,
                "stop managed llama.cpp process",
                self.controller.terminate(&current.handle, context),
            )
            .await?;
            self.clear_residency_if(&current.handle.process_id)?;
        }
        Ok(current)
    }

    async fn status_impl(
        &self,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<RuntimeStatus> {
        context.preflight("query managed llama.cpp status")?;
        let Some(residency) = self.current_residency()? else {
            return Ok(RuntimeStatus {
                runtime: self.descriptor.clone(),
                state: RuntimeLifecycleState::Stopped,
                version: None,
                process: None,
                message: None,
                checked_at_ms: now_ms(),
            });
        };
        let process = self.inspect_residency(&residency, context).await?;
        let state = match process.state {
            ManagedProcessState::Starting => RuntimeLifecycleState::Starting,
            ManagedProcessState::Ready => RuntimeLifecycleState::Ready,
            ManagedProcessState::Exited => RuntimeLifecycleState::Stopped,
            ManagedProcessState::Failed => RuntimeLifecycleState::Error,
        };
        if matches!(
            process.state,
            ManagedProcessState::Exited | ManagedProcessState::Failed
        ) {
            self.clear_residency_if(&residency.handle.process_id)?;
        }
        Ok(RuntimeStatus {
            runtime: self.descriptor.clone(),
            state,
            version: None,
            process: Some(process.handle),
            message: process.message,
            checked_at_ms: now_ms(),
        })
    }

    async fn load_impl(
        &self,
        request: &ModelLoadRequest,
        context: &RuntimeOperationContext,
    ) -> RuntimeAdapterResult<ModelLoadOutcome> {
        context.preflight("load managed llama.cpp model")?;
        validate_model_id(&request.model_id)?;
        if request.keep_alive.is_some() {
            return Err(self.unsupported("keep-alive"));
        }
        validate_setting_values(
            &self.descriptor.runtime_id,
            &self.capabilities.settings,
            &request.settings,
            context.limits.max_config_bytes,
        )?;
        let model = self
            .models
            .iter()
            .find(|model| model.model_id == request.model_id)
            .cloned()
            .ok_or_else(|| RuntimeAdapterError::ModelNotFound {
                runtime_id: self.descriptor.runtime_id.clone(),
                model_id: request.model_id.clone(),
            })?;
        let model_path =
            model
                .local_path
                .as_ref()
                .ok_or_else(|| RuntimeAdapterError::ModelPathUnavailable {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                })?;
        let model_path =
            model_path
                .to_str()
                .ok_or_else(|| RuntimeAdapterError::ModelPathUnavailable {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                })?;

        if let Some(current) = self.current_residency()? {
            let status = self.inspect_residency(&current, context).await?;
            if current.model.model_id == request.model_id
                && matches!(
                    status.state,
                    ManagedProcessState::Starting | ManagedProcessState::Ready
                )
            {
                return Ok(ModelLoadOutcome {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                    disposition: ModelLoadDisposition::AlreadyResident,
                    ownership: ResidencyOwnership::AppManaged,
                });
            }
            if matches!(
                status.state,
                ManagedProcessState::Exited | ManagedProcessState::Failed
            ) {
                self.clear_residency_if(&current.handle.process_id)?;
            } else if !request.replace_existing {
                return Err(RuntimeAdapterError::ProcessSlotBusy {
                    slot_id: self.descriptor.runtime_id.clone(),
                    model_id: current.model.model_id,
                });
            } else {
                self.stop_current(context).await?;
            }
        }

        let port_owner = bounded_operation(
            context,
            "check managed llama.cpp port",
            self.controller.port_owner(self.port, context),
        )
        .await?;
        ensure_serialized_response_size(
            &port_owner,
            context.limits.max_response_bytes,
            "check managed llama.cpp port",
        )?;
        if let Some(owner) = port_owner {
            return Err(RuntimeAdapterError::PortCollision {
                port: self.port,
                owner_id: owner.owner_id,
            });
        }

        // Resolve the speculative-decoding draft model (if any) to a real
        // file path from this adapter's own configured model list — the
        // same source of truth `model_path` above was just resolved from.
        // Whether the requested draft model id is actually a *compatible*
        // draft for `request.model_id` (same family, smaller, installed) is
        // a Runtime Hub concept enforced before this call ever happens (see
        // `M3RuntimeHub::load_model`'s draft-model gate); this only needs to
        // turn a known model id into a path or fail clearly if it isn't one.
        let draft_model_path = match request.settings.get("speculative_decoding_draft_model") {
            Some(SettingValue::Text { value }) if !value.is_empty() => {
                let draft_model = self
                    .models
                    .iter()
                    .find(|candidate| candidate.model_id == *value)
                    .ok_or_else(|| RuntimeAdapterError::ModelNotFound {
                        runtime_id: self.descriptor.runtime_id.clone(),
                        model_id: value.clone(),
                    })?;
                let path = draft_model.local_path.as_ref().ok_or_else(|| {
                    RuntimeAdapterError::ModelPathUnavailable {
                        runtime_id: self.descriptor.runtime_id.clone(),
                        model_id: value.clone(),
                    }
                })?;
                let path = path
                    .to_str()
                    .ok_or_else(|| RuntimeAdapterError::ModelPathUnavailable {
                        runtime_id: self.descriptor.runtime_id.clone(),
                        model_id: value.clone(),
                    })?;
                Some(path.to_string())
            }
            _ => None,
        };

        let args = llama_args(
            model_path,
            self.port,
            &request.settings,
            draft_model_path.as_deref(),
        )?;
        let spec = ManagedProcessSpec {
            runtime_id: self.descriptor.runtime_id.clone(),
            program: self.executable.clone(),
            args,
            port: self.port,
        };
        spec.validate(context.limits.max_config_bytes)?;
        let handle = bounded_operation(
            context,
            "launch managed llama.cpp process",
            self.controller.launch(spec, context),
        )
        .await?;
        ensure_serialized_response_size(
            &handle,
            context.limits.max_response_bytes,
            "launch managed llama.cpp process",
        )?;
        if handle.port != self.port || handle.process_id.trim().is_empty() {
            return Err(RuntimeAdapterError::Controller {
                operation: "launch managed llama.cpp process".to_string(),
                message: "controller returned a mismatched port or empty process id".to_string(),
            });
        }
        *lock(&self.residency)? = Some(ManagedLlamaResidency { model, handle });
        Ok(ModelLoadOutcome {
            runtime_id: self.descriptor.runtime_id.clone(),
            model_id: request.model_id.clone(),
            disposition: ModelLoadDisposition::Loaded,
            ownership: ResidencyOwnership::AppManaged,
        })
    }
}

impl RuntimeAdapter for ManagedLlamaCppAdapter {
    fn descriptor(&self) -> RuntimeDescriptor {
        self.descriptor.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        self.capabilities.clone()
    }

    fn status<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move { self.status_impl(context).await })
    }

    fn inventory<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            context.preflight("list managed llama.cpp model inventory")?;
            Ok(RuntimeInventory {
                schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id: self.descriptor.runtime_id.clone(),
                models: self.models.clone(),
                captured_at_ms: now_ms(),
            })
        })
    }

    fn running_models<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Vec<RunningModel>> {
        Box::pin(async move {
            context.preflight("list managed llama.cpp running models")?;
            let Some(residency) = self.current_residency()? else {
                return Ok(Vec::new());
            };
            let status = self.inspect_residency(&residency, context).await?;
            if matches!(
                status.state,
                ManagedProcessState::Exited | ManagedProcessState::Failed
            ) {
                self.clear_residency_if(&residency.handle.process_id)?;
                return Ok(Vec::new());
            }
            Ok(vec![RunningModel {
                runtime_id: self.descriptor.runtime_id.clone(),
                model_id: residency.model.model_id,
                size_bytes: residency.model.size_bytes,
                memory_bytes: residency.model.size_bytes,
                vram_bytes: 0,
                digest: residency.model.digest,
                expires_at: None,
                ownership: ResidencyOwnership::AppManaged,
            }])
        })
    }

    fn start<'a>(
        &'a self,
        request: &'a RuntimeStartRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move {
            context.preflight("start managed llama.cpp runtime")?;
            let _mutation_guard = async_lock(
                &self.mutation_lock,
                context,
                "queue managed llama.cpp start",
            )
            .await?;
            let initial_model = request.initial_model.as_ref().ok_or_else(|| {
                RuntimeAdapterError::InvalidProcessSpec {
                    message: "managed llama.cpp start requires an initial model".to_string(),
                }
            })?;
            self.load_impl(initial_model, context).await?;
            self.status_impl(context).await
        })
    }

    fn stop<'a>(
        &'a self,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeStatus> {
        Box::pin(async move {
            context.preflight("stop managed llama.cpp runtime")?;
            let _mutation_guard =
                async_lock(&self.mutation_lock, context, "queue managed llama.cpp stop").await?;
            self.stop_current(context).await?;
            Ok(RuntimeStatus {
                runtime: self.descriptor.clone(),
                state: RuntimeLifecycleState::Stopped,
                version: None,
                process: None,
                message: None,
                checked_at_ms: now_ms(),
            })
        })
    }

    fn load_model<'a>(
        &'a self,
        request: &'a ModelLoadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelLoadOutcome> {
        Box::pin(async move {
            let _mutation_guard = async_lock(
                &self.mutation_lock,
                context,
                "queue managed llama.cpp model load",
            )
            .await?;
            self.load_impl(request, context).await
        })
    }

    fn unload_model<'a>(
        &'a self,
        request: &'a ModelUnloadRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ModelUnloadOutcome> {
        Box::pin(async move {
            context.preflight("unload managed llama.cpp model")?;
            let _mutation_guard = async_lock(
                &self.mutation_lock,
                context,
                "queue managed llama.cpp model unload",
            )
            .await?;
            validate_model_id(&request.model_id)?;
            let Some(current) = self.current_residency()? else {
                return Ok(ModelUnloadOutcome {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                    disposition: ModelUnloadDisposition::NotRunning,
                });
            };
            if current.model.model_id != request.model_id {
                return Ok(ModelUnloadOutcome {
                    runtime_id: self.descriptor.runtime_id.clone(),
                    model_id: request.model_id.clone(),
                    disposition: ModelUnloadDisposition::NotRunning,
                });
            }
            bounded_operation(
                context,
                "unload managed llama.cpp model",
                self.controller.terminate(&current.handle, context),
            )
            .await?;
            self.clear_residency_if(&current.handle.process_id)?;
            Ok(ModelUnloadOutcome {
                runtime_id: self.descriptor.runtime_id.clone(),
                model_id: request.model_id.clone(),
                disposition: ModelUnloadDisposition::Unloaded,
            })
        })
    }

    fn set_keep_alive<'a>(
        &'a self,
        _request: &'a KeepAliveRequest,
        _context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move { Err(self.unsupported("keep-alive")) })
    }

    fn tail_logs<'a>(
        &'a self,
        request: &'a RuntimeLogRequest,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            context.preflight("tail managed llama.cpp logs")?;
            if request.max_bytes == 0 || request.max_bytes > context.limits.max_log_bytes {
                return Err(RuntimeAdapterError::LogTooLarge {
                    limit: context.limits.max_log_bytes,
                    actual: request.max_bytes,
                });
            }
            let current =
                self.current_residency()?
                    .ok_or_else(|| RuntimeAdapterError::ModelNotRunning {
                        runtime_id: self.descriptor.runtime_id.clone(),
                        model_id: "<managed-slot>".to_string(),
                    })?;
            let chunk = bounded_operation(
                context,
                "tail managed llama.cpp logs",
                self.controller
                    .tail_logs(&current.handle, request.max_bytes, context),
            )
            .await?;
            if chunk.text.len() > request.max_bytes
                || chunk.text.len() > context.limits.max_log_bytes
            {
                return Err(RuntimeAdapterError::LogTooLarge {
                    limit: request.max_bytes.min(context.limits.max_log_bytes),
                    actual: chunk.text.len(),
                });
            }
            Ok(RuntimeLogTail {
                text: chunk.text,
                truncated: chunk.truncated,
            })
        })
    }
}

fn llama_setting_capabilities() -> Vec<AdvancedSettingCapability> {
    vec![
        AdvancedSettingCapability {
            key: "context_size".to_string(),
            label: "Context size".to_string(),
            description: "llama-server context window.".to_string(),
            schema: SettingValueSchema::Integer {
                min: 128,
                max: 1_048_576,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 4_096 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "gpu_layers".to_string(),
            label: "GPU layers".to_string(),
            description: "Number of layers offloaded by llama-server.".to_string(),
            schema: SettingValueSchema::Integer {
                min: -1,
                max: 999,
                step: 1,
            },
            default_value: SettingValue::Integer { value: -1 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "threads".to_string(),
            label: "CPU threads".to_string(),
            description: "Worker threads used by llama-server.".to_string(),
            schema: SettingValueSchema::Integer {
                min: 1,
                max: 1_024,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 4 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "flash_attention".to_string(),
            label: "Flash attention".to_string(),
            description: "Select llama.cpp flash-attention behavior. Needs a supported GPU backend; gated dynamically by the Runtime Hub against the Hardware Compatibility report (see `m3_runtime_hub.rs`'s `gate_advanced_settings`).".to_string(),
            schema: SettingValueSchema::Choice {
                options: vec!["auto".to_string(), "on".to_string(), "off".to_string()],
            },
            default_value: SettingValue::Choice {
                value: "auto".to_string(),
            },
            restart_required: true,
            // Baseline: this adapter always knows how to pass `--flash-attn`.
            // Whether "on" can actually be honored depends on hardware the
            // low-level adapter has no visibility into — see the Runtime Hub
            // gating layer, which narrows this per-machine.
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "embeddings".to_string(),
            label: "Embeddings".to_string(),
            description: "Enable the embeddings endpoint.".to_string(),
            schema: SettingValueSchema::Boolean,
            default_value: SettingValue::Boolean { value: false },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        // -- Sampler, batching, speculative decoding, and mixed precision
        // controls (ROADMAP Phase 8 item 17) --
        AdvancedSettingCapability {
            key: "temperature".to_string(),
            label: "Temperature".to_string(),
            description: "Default sampling temperature (`--temp`).".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 2.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.8 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "top_p".to_string(),
            label: "Top-p".to_string(),
            description: "Nucleus sampling probability mass cutoff (`--top-p`).".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 1.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.9 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "top_k".to_string(),
            label: "Top-k".to_string(),
            description: "Limits sampling to the k most likely next tokens (`--top-k`)."
                .to_string(),
            schema: SettingValueSchema::Integer {
                min: 0,
                max: 1_000,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 40 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "repeat_penalty".to_string(),
            label: "Repeat penalty".to_string(),
            description: "Penalizes tokens that already appeared in the context (`--repeat-penalty`).".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 2.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 1.1 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "min_p".to_string(),
            label: "Min-p".to_string(),
            description: "Minimum token probability, relative to the most likely token (`--min-p`).".to_string(),
            schema: SettingValueSchema::Float {
                min: 0.0,
                max: 1.0,
                step: 0.01,
            },
            default_value: SettingValue::Float { value: 0.05 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "batch_size".to_string(),
            label: "Batch size".to_string(),
            description: "Logical batch size llama-server processes per step (`--batch-size`)."
                .to_string(),
            schema: SettingValueSchema::Integer {
                min: 1,
                max: 8_192,
                step: 1,
            },
            default_value: SettingValue::Integer { value: 2_048 },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "mixed_precision".to_string(),
            label: "Mixed precision (KV cache)".to_string(),
            description: "Quantizes the K/V cache (`--cache-type-k`/`--cache-type-v`) to trade a little quality for memory; llama.cpp requires flash attention for anything below f16, so this needs a supported GPU backend too. Gated dynamically by the Runtime Hub against the Hardware Compatibility report.".to_string(),
            schema: SettingValueSchema::Choice {
                options: vec!["f16".to_string(), "q8_0".to_string(), "q4_0".to_string()],
            },
            default_value: SettingValue::Choice {
                value: "f16".to_string(),
            },
            restart_required: true,
            supported: true,
            unsupported_reason: None,
        },
        AdvancedSettingCapability {
            key: "speculative_decoding_draft_model".to_string(),
            label: "Speculative decoding draft model".to_string(),
            description: "Model id of a smaller, same-family installed model to use as a speculative-decoding draft (`--model-draft`). Empty disables speculative decoding.".to_string(),
            // A fixed `Choice` schema cannot express "whichever installed
            // models are currently a compatible draft for whichever model
            // gets loaded" — that set only exists relative to a specific
            // target model, which this adapter has no notion of. So this is
            // a plain model-id string; which ids are actually valid right
            // now is a Runtime Hub concept (`gate_advanced_settings` +
            // `M3SettingCapabilitiesView::draft_model_candidates`) surfaced
            // to the UI as a separate candidate list, and enforced
            // authoritatively at load time (`M3RuntimeHub::load_model`)
            // regardless of what the UI showed.
            schema: SettingValueSchema::Text { max_bytes: 256 },
            default_value: SettingValue::Text {
                value: String::new(),
            },
            restart_required: true,
            // Baseline: unknown until a target model is selected, so this
            // control starts disabled. The Runtime Hub layer flips it to
            // `true` once it finds at least one compatible installed draft
            // model for the model currently being configured.
            supported: false,
            unsupported_reason: Some(
                "Select a model to check for a compatible installed draft model.".to_string(),
            ),
        },
    ]
}

fn llama_args(
    model_path: &str,
    port: u16,
    settings: &BTreeMap<String, SettingValue>,
    draft_model_path: Option<&str>,
) -> RuntimeAdapterResult<Vec<String>> {
    let context_size = integer_setting(settings, "context_size", 4_096)?;
    let gpu_layers = integer_setting(settings, "gpu_layers", -1)?;
    let threads = settings
        .get("threads")
        .map(|_| integer_setting(settings, "threads", 4))
        .transpose()?;
    let flash_attention = choice_setting(settings, "flash_attention", "auto")?;
    let embeddings = boolean_setting(settings, "embeddings", false)?;
    let mut args = vec![
        "-m".to_string(),
        model_path.to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "-c".to_string(),
        context_size.to_string(),
        "-ngl".to_string(),
        gpu_layers.to_string(),
        "--jinja".to_string(),
    ];
    if let Some(threads) = threads {
        args.extend(["-t".to_string(), threads.to_string()]);
    }
    if settings.contains_key("flash_attention") {
        args.extend(["--flash-attn".to_string(), flash_attention]);
    }
    if embeddings {
        args.push("--embeddings".to_string());
    }
    if settings.contains_key("temperature") {
        args.extend([
            "--temp".to_string(),
            float_setting(settings, "temperature", 0.8)?.to_string(),
        ]);
    }
    if settings.contains_key("top_p") {
        args.extend([
            "--top-p".to_string(),
            float_setting(settings, "top_p", 0.9)?.to_string(),
        ]);
    }
    if settings.contains_key("top_k") {
        args.extend([
            "--top-k".to_string(),
            integer_setting(settings, "top_k", 40)?.to_string(),
        ]);
    }
    if settings.contains_key("repeat_penalty") {
        args.extend([
            "--repeat-penalty".to_string(),
            float_setting(settings, "repeat_penalty", 1.1)?.to_string(),
        ]);
    }
    if settings.contains_key("min_p") {
        args.extend([
            "--min-p".to_string(),
            float_setting(settings, "min_p", 0.05)?.to_string(),
        ]);
    }
    if settings.contains_key("batch_size") {
        args.extend([
            "--batch-size".to_string(),
            integer_setting(settings, "batch_size", 2_048)?.to_string(),
        ]);
    }
    if settings.contains_key("mixed_precision") {
        let cache_type = choice_setting(settings, "mixed_precision", "f16")?;
        args.extend(["--cache-type-k".to_string(), cache_type.clone()]);
        args.extend(["--cache-type-v".to_string(), cache_type]);
    }
    if let Some(draft_model_path) = draft_model_path {
        args.extend(["--model-draft".to_string(), draft_model_path.to_string()]);
    }
    Ok(args)
}

fn integer_setting(
    settings: &BTreeMap<String, SettingValue>,
    key: &str,
    default: i64,
) -> RuntimeAdapterResult<i64> {
    match settings.get(key) {
        Some(SettingValue::Integer { value }) => Ok(*value),
        Some(_) => Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "expected an integer".to_string(),
        }),
        None => Ok(default),
    }
}

fn boolean_setting(
    settings: &BTreeMap<String, SettingValue>,
    key: &str,
    default: bool,
) -> RuntimeAdapterResult<bool> {
    match settings.get(key) {
        Some(SettingValue::Boolean { value }) => Ok(*value),
        Some(_) => Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "expected a boolean".to_string(),
        }),
        None => Ok(default),
    }
}

fn choice_setting(
    settings: &BTreeMap<String, SettingValue>,
    key: &str,
    default: &str,
) -> RuntimeAdapterResult<String> {
    match settings.get(key) {
        Some(SettingValue::Choice { value }) => Ok(value.clone()),
        Some(_) => Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "expected a supported choice".to_string(),
        }),
        None => Ok(default.to_string()),
    }
}

fn float_setting(
    settings: &BTreeMap<String, SettingValue>,
    key: &str,
    default: f64,
) -> RuntimeAdapterResult<f64> {
    match settings.get(key) {
        Some(SettingValue::Float { value }) => Ok(*value),
        Some(_) => Err(RuntimeAdapterError::InvalidSetting {
            key: key.to_string(),
            message: "expected a float".to_string(),
        }),
        None => Ok(default),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryRequirement {
    pub ram_bytes: u64,
    pub vram_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryBudget {
    /// Currently available memory. Existing residents are already reflected
    /// in these values and therefore must not be subtracted a second time.
    pub available_ram_bytes: u64,
    pub reserve_ram_bytes: u64,
    pub available_vram_bytes: u64,
    pub reserve_vram_bytes: u64,
}

impl MemoryBudget {
    pub fn schedulable_ram_bytes(&self) -> u64 {
        self.available_ram_bytes
            .saturating_sub(self.reserve_ram_bytes)
    }

    pub fn schedulable_vram_bytes(&self) -> u64 {
        self.available_vram_bytes
            .saturating_sub(self.reserve_vram_bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessSlotState {
    Available,
    Occupied {
        model_id: String,
        ownership: ResidencyOwnership,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessSlot {
    pub slot_id: String,
    pub runtime: RuntimeKind,
    pub port: Option<u16>,
    pub state: ProcessSlotState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResidentModelAllocation {
    pub runtime: RuntimeKind,
    pub model_id: String,
    pub memory: MemoryRequirement,
    pub ownership: ResidencyOwnership,
    pub slot_id: Option<String>,
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTarget {
    pub target_id: String,
    pub runtime: RuntimeKind,
    pub model_id: String,
    pub memory: MemoryRequirement,
    pub accelerator: Option<AcceleratorKind>,
    pub preferred_slot_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulingInput {
    pub platform: PlatformCapabilities,
    pub memory: MemoryBudget,
    pub process_slots: Vec<ProcessSlot>,
    pub residents: Vec<ResidentModelAllocation>,
    pub ports: Vec<PortOwnership>,
    pub targets: Vec<ScheduleTarget>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledResidency {
    ReuseExisting,
    LoadTransient,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledCleanup {
    Preserve,
    UnloadAppManaged,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTarget {
    pub target_id: String,
    pub runtime: RuntimeKind,
    pub model_id: String,
    pub process_slot_id: Option<String>,
    pub port: Option<u16>,
    pub residency: ScheduledResidency,
    pub cleanup: ScheduledCleanup,
    /// `true` means an earlier wave must complete and release its transient
    /// resources before this target starts.
    pub queued: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleWave {
    pub wave_index: usize,
    pub ram_bytes: u64,
    pub vram_bytes: u64,
    pub targets: Vec<ScheduledTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulingPlan {
    pub schema_version: u32,
    pub waves: Vec<ScheduleWave>,
    pub preserved_residency: Vec<ResidentModelAllocation>,
}

pub struct LocalRuntimeScheduler;

impl LocalRuntimeScheduler {
    /// Plans transient execution waves. Every newly loaded model is paired
    /// with `UnloadAppManaged`, so memory/process slots become reusable in the
    /// next wave. Existing residents are reused and always marked `Preserve`.
    pub fn plan(input: &SchedulingInput) -> RuntimeAdapterResult<SchedulingPlan> {
        validate_schedule_input(input)?;
        let available_ram = input.memory.schedulable_ram_bytes();
        let available_vram = input.memory.schedulable_vram_bytes();
        let preserved_residency = input
            .residents
            .iter()
            .filter(|resident| resident.ownership != ResidencyOwnership::AppManaged)
            .cloned()
            .collect();
        let mut waves: Vec<ScheduleWave> = Vec::new();

        for target in &input.targets {
            if let Some(accelerator) = target.accelerator {
                if !input.platform.supports_accelerator(accelerator) {
                    return Err(RuntimeAdapterError::IncompatiblePlatform {
                        target_id: target.target_id.clone(),
                        accelerator,
                    });
                }
            }

            let resident = input.residents.iter().find(|resident| {
                resident.runtime == target.runtime
                    && resident.model_id == target.model_id
                    && target
                        .preferred_slot_id
                        .as_ref()
                        .is_none_or(|preferred| resident.slot_id.as_ref() == Some(preferred))
            });
            if let Some(resident) = resident {
                ensure_wave_zero(&mut waves);
                waves[0].targets.push(ScheduledTarget {
                    target_id: target.target_id.clone(),
                    runtime: target.runtime,
                    model_id: target.model_id.clone(),
                    process_slot_id: resident.slot_id.clone(),
                    port: resident.port,
                    residency: ScheduledResidency::ReuseExisting,
                    cleanup: ScheduledCleanup::Preserve,
                    queued: false,
                });
                continue;
            }

            if target.memory.ram_bytes > available_ram || target.memory.vram_bytes > available_vram
            {
                return Err(RuntimeAdapterError::InsufficientMemory {
                    target_id: target.target_id.clone(),
                    required_ram_bytes: target.memory.ram_bytes,
                    available_ram_bytes: available_ram,
                    required_vram_bytes: target.memory.vram_bytes,
                    available_vram_bytes: available_vram,
                });
            }

            let candidates: Vec<&ProcessSlot> = input
                .process_slots
                .iter()
                .filter(|slot| {
                    slot.runtime == target.runtime
                        && matches!(slot.state, ProcessSlotState::Available)
                        && target
                            .preferred_slot_id
                            .as_ref()
                            .is_none_or(|preferred| preferred == &slot.slot_id)
                })
                .collect();
            if candidates.is_empty() {
                return Err(RuntimeAdapterError::NoCompatibleProcessSlot {
                    target_id: target.target_id.clone(),
                    runtime: target.runtime,
                });
            }
            let unblocked: Vec<&ProcessSlot> = candidates
                .iter()
                .copied()
                .filter(|slot| !port_is_owned_by_other(slot.port, &target.target_id, &input.ports))
                .collect();
            if unblocked.is_empty() {
                let port = candidates.iter().find_map(|slot| slot.port).unwrap_or(0);
                let owner_id = input
                    .ports
                    .iter()
                    .find(|owner| owner.port == port)
                    .map(|owner| owner.owner_id.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                return Err(RuntimeAdapterError::PortCollision { port, owner_id });
            }

            let mut placement = None;
            for wave_index in 0..=waves.len() {
                if wave_index == waves.len() {
                    waves.push(ScheduleWave {
                        wave_index,
                        ram_bytes: 0,
                        vram_bytes: 0,
                        targets: Vec::new(),
                    });
                }
                let wave = &waves[wave_index];
                if wave.ram_bytes.saturating_add(target.memory.ram_bytes) > available_ram
                    || wave.vram_bytes.saturating_add(target.memory.vram_bytes) > available_vram
                {
                    continue;
                }
                let slot = unblocked.iter().copied().find(|slot| {
                    !wave.targets.iter().any(|scheduled| {
                        scheduled.process_slot_id.as_deref() == Some(slot.slot_id.as_str())
                            || (slot.port.is_some() && scheduled.port == slot.port)
                    })
                });
                if let Some(slot) = slot {
                    placement = Some((wave_index, slot.clone()));
                    break;
                }
            }
            let (wave_index, slot) =
                placement.ok_or_else(|| RuntimeAdapterError::NoCompatibleProcessSlot {
                    target_id: target.target_id.clone(),
                    runtime: target.runtime,
                })?;
            let wave = &mut waves[wave_index];
            wave.ram_bytes = wave.ram_bytes.saturating_add(target.memory.ram_bytes);
            wave.vram_bytes = wave.vram_bytes.saturating_add(target.memory.vram_bytes);
            wave.targets.push(ScheduledTarget {
                target_id: target.target_id.clone(),
                runtime: target.runtime,
                model_id: target.model_id.clone(),
                process_slot_id: Some(slot.slot_id),
                port: slot.port,
                residency: ScheduledResidency::LoadTransient,
                cleanup: ScheduledCleanup::UnloadAppManaged,
                queued: wave_index > 0,
            });
        }

        while waves.last().is_some_and(|wave| wave.targets.is_empty()) {
            waves.pop();
        }
        Ok(SchedulingPlan {
            schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
            waves,
            preserved_residency,
        })
    }
}

fn ensure_wave_zero(waves: &mut Vec<ScheduleWave>) {
    if waves.is_empty() {
        waves.push(ScheduleWave {
            wave_index: 0,
            ram_bytes: 0,
            vram_bytes: 0,
            targets: Vec::new(),
        });
    }
}

fn port_is_owned_by_other(port: Option<u16>, target_id: &str, ownership: &[PortOwnership]) -> bool {
    port.is_some_and(|port| {
        ownership
            .iter()
            .any(|owner| owner.port == port && owner.owner_id != target_id)
    })
}

fn validate_schedule_input(input: &SchedulingInput) -> RuntimeAdapterResult<()> {
    if input.memory.reserve_ram_bytes > input.memory.available_ram_bytes
        || input.memory.reserve_vram_bytes > input.memory.available_vram_bytes
    {
        return Err(RuntimeAdapterError::InvalidOperationLimits {
            message: "scheduler reserves cannot exceed available memory".to_string(),
        });
    }
    if input.targets.len() > MAX_MODELS_PER_RESPONSE {
        return Err(RuntimeAdapterError::ConfigTooLarge {
            limit: MAX_MODELS_PER_RESPONSE,
            actual: input.targets.len(),
        });
    }
    let mut target_ids = BTreeSet::new();
    for target in &input.targets {
        validate_runtime_id(&target.target_id)?;
        validate_model_id(&target.model_id)?;
        if !target_ids.insert(target.target_id.clone()) {
            return Err(RuntimeAdapterError::InvalidIdentifier {
                field: "target_id",
                value: target.target_id.clone(),
            });
        }
    }
    let mut slot_ids = BTreeSet::new();
    for slot in &input.process_slots {
        validate_runtime_id(&slot.slot_id)?;
        if slot.port == Some(0) || !slot_ids.insert(slot.slot_id.clone()) {
            return Err(RuntimeAdapterError::InvalidIdentifier {
                field: "process_slot",
                value: slot.slot_id.clone(),
            });
        }
        if let ProcessSlotState::Occupied { model_id, .. } = &slot.state {
            validate_model_id(model_id)?;
        }
    }
    let mut ports = BTreeSet::new();
    for owner in &input.ports {
        if owner.port == 0 || !ports.insert(owner.port) {
            return Err(RuntimeAdapterError::PortCollision {
                port: owner.port,
                owner_id: owner.owner_id.clone(),
            });
        }
        validate_runtime_id(&owner.owner_id)?;
    }
    for resident in &input.residents {
        validate_model_id(&resident.model_id)?;
        if resident.port == Some(0) {
            return Err(RuntimeAdapterError::InvalidProcessSpec {
                message: "resident model cannot own port zero".to_string(),
            });
        }
    }
    Ok(())
}

fn validate_runtime_id(value: &str) -> RuntimeAdapterResult<()> {
    validate_ascii_identifier("runtime_id", value, 128)
}

fn validate_model_id(value: &str) -> RuntimeAdapterResult<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && !value.contains("..")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
        });
    if valid {
        Ok(())
    } else {
        Err(RuntimeAdapterError::InvalidIdentifier {
            field: "model_id",
            value: value.to_string(),
        })
    }
}

fn validate_ascii_identifier(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> RuntimeAdapterResult<()> {
    let valid = !value.is_empty()
        && value.len() <= max_bytes
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(RuntimeAdapterError::InvalidIdentifier {
            field,
            value: value.to_string(),
        })
    }
}

fn ensure_serialized_response_size<T: Serialize>(
    value: &T,
    limit: usize,
    operation: &str,
) -> RuntimeAdapterResult<()> {
    let encoded =
        serde_json::to_vec(value).map_err(|error| RuntimeAdapterError::MalformedResponse {
            operation: operation.to_string(),
            message: error.to_string(),
        })?;
    if encoded.len() > limit {
        Err(RuntimeAdapterError::ResponseTooLarge {
            limit,
            actual_at_least: encoded.len(),
        })
    } else {
        Ok(())
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> RuntimeAdapterResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| RuntimeAdapterError::LockPoisoned)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Adaptive per-load offload planner (ROADMAP Phase 8, "Adaptive Runtime
// Scheduler and Offload Planner").
//
// `LocalRuntimeScheduler` above decides *which* models can run concurrently
// and *when* (execution waves) from whole-model memory estimates. It does not
// decide *how* a single model load should be configured. `LocalOffloadPlanner`
// fills that gap: given a live hardware snapshot and one model's memory
// profile, it recommends a context window, batch size, GPU layer offload
// count, projector placement, CPU spill, and parallelism, together with a
// short rationale per field and concrete improvement suggestions. It is pure,
// deterministic, and advisory only: it never starts, stops, or reconfigures a
// runtime process.
// ---------------------------------------------------------------------------

/// Context length assumed to already be reflected in a catalog or installed
/// model's `estimated_ram_bytes`/`estimated_vram_bytes` figures. Those figures
/// come from whole-model measurements taken by catalog maintainers rather
/// than GGUF/safetensors metadata this planner can read directly, so the
/// KV-cache contribution at other context lengths is scaled from this
/// assumed baseline.
const OFFLOAD_BASELINE_CONTEXT_TOKENS: u32 = 4_096;
const OFFLOAD_DEFAULT_CONTEXT_TOKENS: u32 = 8_192;
const OFFLOAD_MIN_CONTEXT_TOKENS: u32 = 512;
const OFFLOAD_MAX_CONTEXT_TOKENS: u32 = 131_072;
const OFFLOAD_CONTEXT_TIERS: &[u32] = &[512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072];
/// Floor for estimated KV-cache bytes per token when a model's baseline
/// footprint does not clearly separate weights from cache overhead.
const OFFLOAD_MIN_KV_BYTES_PER_TOKEN: u64 = 8 * 1024;
const OFFLOAD_MAX_PARALLEL_SEQUENCES: u16 = 8;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OffloadModelProfile {
    /// Exact on-disk weight size for the selected model artifact.
    pub weights_bytes: u64,
    /// Estimated total footprint (weights + baseline KV/overhead) if this
    /// model runs entirely on CPU/RAM at `OFFLOAD_BASELINE_CONTEXT_TOKENS`.
    pub estimated_ram_bytes: u64,
    /// Estimated total footprint if this model runs fully offloaded to an
    /// accelerator at `OFFLOAD_BASELINE_CONTEXT_TOKENS`. Zero when the model
    /// has no meaningful GPU/Metal offload path.
    pub estimated_vram_bytes: u64,
    pub required_accelerator: Option<AcceleratorKind>,
    pub has_vision_projector: bool,
    /// Estimated resident memory the multimodal projector itself needs once
    /// loaded (ROADMAP Phase 8 item 12), separate from `weights_bytes`/
    /// `estimated_ram_bytes`/`estimated_vram_bytes` above, which describe the
    /// base language model only. Ignored when `has_vision_projector` is
    /// false. Zero is a legitimate value for a vision-capable model whose
    /// projector size is not yet known, in which case this planner simply
    /// reserves nothing extra for it (see `m3_runtime_hub::
    /// estimated_projector_memory_bytes` for how a real figure is derived
    /// from a catalog's declared `M3ProjectorRef`).
    pub projector_memory_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OffloadPlanInput {
    pub hardware: HardwareSnapshot,
    pub model: OffloadModelProfile,
    /// Memory already committed by other models resident right now; treated
    /// as unavailable headroom without double-subtracting it from
    /// `hardware`'s own available counters.
    pub reserved: MemoryRequirement,
    pub other_resident_count: u32,
    /// Desired context window; defaults to `OFFLOAD_DEFAULT_CONTEXT_TOKENS`
    /// when omitted. The plan may recommend a smaller value than requested.
    pub requested_context_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectorPlacement {
    Gpu,
    Cpu,
    NotApplicable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OffloadRationale {
    pub field: String,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OffloadPlan {
    pub schema_version: u32,
    pub accelerator: AcceleratorKind,
    pub context_tokens: u32,
    pub requested_context_tokens: u32,
    pub batch_size: u32,
    pub gpu_layers: u32,
    pub estimated_total_layers: u32,
    pub cpu_spill_layers: u32,
    pub projector_placement: ProjectorPlacement,
    pub parallel_sequences: u16,
    pub available_ram_bytes: u64,
    pub available_vram_bytes: u64,
    pub rationale: Vec<OffloadRationale>,
    pub improvement_suggestions: Vec<String>,
}

pub struct LocalOffloadPlanner;

impl LocalOffloadPlanner {
    /// Simulates fit and computes a per-load offload plan before a model is
    /// actually loaded. Pure and deterministic: identical inputs always
    /// produce identical output, which keeps it unit-testable without any
    /// real GPU hardware.
    pub fn plan(input: &OffloadPlanInput) -> RuntimeAdapterResult<OffloadPlan> {
        let profile = input.hardware.profile()?;
        validate_offload_plan_input(input)?;

        let mut rationale: Vec<OffloadRationale> = Vec::new();
        let mut improvements: Vec<String> = Vec::new();

        let accelerator = resolve_offload_accelerator(
            &input.model,
            &input.hardware,
            &profile,
            &mut rationale,
            &mut improvements,
        );

        let mut available_ram_bytes = input
            .hardware
            .available_ram_bytes
            .saturating_sub(profile.recommended_ram_reserve_bytes)
            .saturating_sub(input.reserved.ram_bytes);
        // Metal is unified memory: there is exactly one physical pool, so its
        // "VRAM" budget is the same already reserve-adjusted `available_ram_bytes`
        // rather than a second, independently reserved figure. That avoids both
        // double-counting the pool and skipping the OS/other-resident reserve on
        // the accelerator side.
        let mut available_vram_bytes = match accelerator {
            AcceleratorKind::Cpu => 0,
            AcceleratorKind::Metal => available_ram_bytes,
            other => input
                .hardware
                .platform
                .accelerators
                .iter()
                .find(|entry| entry.kind == other && entry.available)
                .and_then(|entry| entry.available_memory_bytes.or(entry.total_memory_bytes))
                .unwrap_or(0)
                .saturating_sub(input.reserved.vram_bytes),
        };

        // Multimodal projector memory sizing (ROADMAP Phase 8 item 12):
        // reserve the projector's own resident footprint off the top of
        // whichever pool it will occupy, *before* the GPU-layer fit fraction
        // and context-tier math below so both genuinely account for it,
        // rather than only deciding *where* the projector goes afterward.
        // Metal keeps both pool variables equal (they represent the same
        // unified memory); a genuinely separate accelerator only spends its
        // own VRAM; CPU-only plans spend system RAM.
        if input.model.has_vision_projector && input.model.projector_memory_bytes > 0 {
            let projector_bytes = input.model.projector_memory_bytes;
            match accelerator {
                AcceleratorKind::Cpu => {
                    available_ram_bytes = available_ram_bytes.saturating_sub(projector_bytes);
                }
                AcceleratorKind::Metal => {
                    available_ram_bytes = available_ram_bytes.saturating_sub(projector_bytes);
                    available_vram_bytes = available_vram_bytes.saturating_sub(projector_bytes);
                }
                _ => {
                    available_vram_bytes = available_vram_bytes.saturating_sub(projector_bytes);
                }
            }
            rationale.push(OffloadRationale {
                field: "projector_memory_bytes".to_string(),
                explanation: format!(
                    "Reserved {} for the multimodal projector's own resident memory before sizing context and GPU layers.",
                    format_bytes_for_rationale(projector_bytes)
                ),
            });
        }

        let estimated_total_layers = estimate_layer_count(input.model.weights_bytes);
        let (gpu_layers, cpu_spill_layers) = if accelerator == AcceleratorKind::Cpu
            || input.model.estimated_vram_bytes == 0
        {
            if accelerator != AcceleratorKind::Cpu {
                rationale.push(OffloadRationale {
                    field: "gpu_layers".to_string(),
                    explanation:
                        "This model has no measured accelerator footprint, so every layer runs on CPU."
                            .to_string(),
                });
            } else {
                rationale.push(OffloadRationale {
                    field: "gpu_layers".to_string(),
                    explanation: format!(
                        "All {estimated_total_layers} estimated layers run on CPU because no compatible accelerator is in use for this load."
                    ),
                });
            }
            (0, estimated_total_layers)
        } else {
            let fit_fraction =
                (available_vram_bytes as f64 / input.model.estimated_vram_bytes as f64).clamp(0.0, 1.0);
            let gpu_layers = ((estimated_total_layers as f64) * fit_fraction).floor() as u32;
            let cpu_spill_layers = estimated_total_layers.saturating_sub(gpu_layers);
            if gpu_layers >= estimated_total_layers {
                rationale.push(OffloadRationale {
                    field: "gpu_layers".to_string(),
                    explanation: format!(
                        "All {estimated_total_layers} estimated layers fit inside the {} of available {accelerator:?} memory.",
                        format_bytes_for_rationale(available_vram_bytes)
                    ),
                });
            } else {
                rationale.push(OffloadRationale {
                    field: "gpu_layers".to_string(),
                    explanation: format!(
                        "{gpu_layers} of {estimated_total_layers} estimated layers fit inside the {} of available {accelerator:?} memory; the remaining {cpu_spill_layers} spill to CPU.",
                        format_bytes_for_rationale(available_vram_bytes)
                    ),
                });
            }
            (gpu_layers, cpu_spill_layers)
        };

        // Metal is unified memory: `available_vram_bytes` already reflects the
        // same physical pool as `available_ram_bytes`, so summing the two
        // would double-count it. Only genuinely separate accelerator memory
        // (CUDA/ROCm/Vulkan/DirectML) adds to the system RAM budget.
        let combined_budget_bytes = match accelerator {
            AcceleratorKind::Cpu => available_ram_bytes,
            AcceleratorKind::Metal => available_vram_bytes,
            _ => available_ram_bytes.saturating_add(available_vram_bytes),
        };

        let kv_bytes_per_token = estimate_kv_bytes_per_token(&input.model);
        let requested_context_tokens = input
            .requested_context_tokens
            .unwrap_or(OFFLOAD_DEFAULT_CONTEXT_TOKENS)
            .clamp(OFFLOAD_MIN_CONTEXT_TOKENS, OFFLOAD_MAX_CONTEXT_TOKENS);

        let budget_after_weights = combined_budget_bytes.saturating_sub(input.model.weights_bytes);
        let max_affordable_context = if kv_bytes_per_token == 0 {
            OFFLOAD_MAX_CONTEXT_TOKENS
        } else {
            let tokens = budget_after_weights / kv_bytes_per_token;
            u32::try_from(tokens.min(u64::from(OFFLOAD_MAX_CONTEXT_TOKENS)))
                .unwrap_or(OFFLOAD_MAX_CONTEXT_TOKENS)
        };
        let context_tokens = pick_context_tier(
            requested_context_tokens.min(max_affordable_context.max(OFFLOAD_MIN_CONTEXT_TOKENS)),
        );

        if context_tokens < requested_context_tokens {
            rationale.push(OffloadRationale {
                field: "context_tokens".to_string(),
                explanation: format!(
                    "Context reduced to {context_tokens} tokens because the {} of remaining memory after weights and other residents only covers about {max_affordable_context} tokens of cache.",
                    format_bytes_for_rationale(combined_budget_bytes)
                ),
            });
        } else {
            rationale.push(OffloadRationale {
                field: "context_tokens".to_string(),
                explanation: format!(
                    "Context set to the requested {context_tokens} tokens; estimated memory after loading remains within budget."
                ),
            });
        }
        if input.model.weights_bytes > combined_budget_bytes {
            improvements.push(format!(
                "Model weights alone ({}) exceed the {} of available memory for this load; free more memory or pick a smaller quantization.",
                format_bytes_for_rationale(input.model.weights_bytes),
                format_bytes_for_rationale(combined_budget_bytes)
            ));
        }

        let kv_bytes_for_chosen_context = kv_bytes_per_token.saturating_mul(u64::from(context_tokens));
        let used_bytes = input.model.weights_bytes.saturating_add(kv_bytes_for_chosen_context);
        let leftover_bytes = combined_budget_bytes.saturating_sub(used_bytes);
        let extra_parallel = if kv_bytes_for_chosen_context == 0 {
            0
        } else {
            leftover_bytes / kv_bytes_for_chosen_context
        };
        let parallel_ceiling = profile.recommended_process_slots.max(1);
        let parallel_sequences = 1u16
            .saturating_add(
                u16::try_from(extra_parallel.min(u64::from(OFFLOAD_MAX_PARALLEL_SEQUENCES - 1)))
                    .unwrap_or(0),
            )
            .min(parallel_ceiling)
            .min(OFFLOAD_MAX_PARALLEL_SEQUENCES);
        rationale.push(OffloadRationale {
            field: "parallel_sequences".to_string(),
            explanation: if parallel_sequences > 1 {
                format!(
                    "{parallel_sequences} concurrent sequences fit because leftover memory after one instance still covers {} more KV cache slot(s), bounded by {parallel_ceiling} recommended process slot(s) for this hardware tier.",
                    parallel_sequences - 1
                )
            } else {
                "Only one sequence fits after the model and its context window are accounted for."
                    .to_string()
            },
        });

        let mut batch_size: u32 = match profile.tier {
            HardwareTier::Constrained => 128,
            HardwareTier::Balanced => 256,
            HardwareTier::Performance => 512,
        };
        if accelerator == AcceleratorKind::Cpu {
            batch_size = batch_size.min(256);
        }
        let headroom_ratio = if combined_budget_bytes == 0 {
            0.0
        } else {
            leftover_bytes as f64 / combined_budget_bytes as f64
        };
        if headroom_ratio < 0.15 {
            let reduced = (batch_size / 2).max(32);
            rationale.push(OffloadRationale {
                field: "batch_size".to_string(),
                explanation: format!(
                    "Batch size reduced from {batch_size} to {reduced} because less than 15% memory headroom remains after context and weights."
                ),
            });
            batch_size = reduced;
        } else {
            rationale.push(OffloadRationale {
                field: "batch_size".to_string(),
                explanation: format!(
                    "Batch size set to {batch_size} for a {:?} hardware tier with comfortable headroom.",
                    profile.tier
                ),
            });
        }

        let projector_placement = if !input.model.has_vision_projector {
            ProjectorPlacement::NotApplicable
        } else if accelerator != AcceleratorKind::Cpu && gpu_layers > 0 {
            rationale.push(OffloadRationale {
                field: "projector_placement".to_string(),
                explanation: format!(
                    "The multimodal projector offloads to {accelerator:?} alongside the offloaded layers."
                ),
            });
            ProjectorPlacement::Gpu
        } else {
            rationale.push(OffloadRationale {
                field: "projector_placement".to_string(),
                explanation:
                    "The multimodal projector runs on CPU because no accelerator layers are offloaded for this load."
                        .to_string(),
            });
            ProjectorPlacement::Cpu
        };

        if input.other_resident_count > 0 && (cpu_spill_layers > 0 || context_tokens < requested_context_tokens)
        {
            improvements.push(format!(
                "Unload {} other resident model{} to free memory and raise the offload/context budget for this load.",
                input.other_resident_count,
                if input.other_resident_count == 1 { "" } else { "s" }
            ));
        }
        if accelerator == AcceleratorKind::Cpu
            && input.model.required_accelerator.is_none()
            && !input
                .hardware
                .platform
                .accelerators
                .iter()
                .any(|entry| entry.available && entry.kind != AcceleratorKind::Cpu)
        {
            improvements.push(
                "No GPU or Metal acceleration was detected on this machine; check the Overview tab for detected accelerators."
                    .to_string(),
            );
        }
        if context_tokens < requested_context_tokens {
            improvements.push(format!(
                "Context is capped at {context_tokens} tokens (requested {requested_context_tokens}); free RAM/VRAM or reduce parallel sequences to raise it."
            ));
        }

        Ok(OffloadPlan {
            schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
            accelerator,
            context_tokens,
            requested_context_tokens,
            batch_size,
            gpu_layers,
            estimated_total_layers,
            cpu_spill_layers,
            projector_placement,
            parallel_sequences,
            available_ram_bytes,
            available_vram_bytes,
            rationale,
            improvement_suggestions: improvements,
        })
    }
}

fn resolve_offload_accelerator(
    model: &OffloadModelProfile,
    hardware: &HardwareSnapshot,
    profile: &HardwareProfile,
    rationale: &mut Vec<OffloadRationale>,
    improvements: &mut Vec<String>,
) -> AcceleratorKind {
    if let Some(required) = model.required_accelerator {
        if hardware.platform.supports_accelerator(required) {
            rationale.push(OffloadRationale {
                field: "accelerator".to_string(),
                explanation: format!(
                    "Using the required {required:?} accelerator, which this machine advertises as available."
                ),
            });
            return required;
        }
        rationale.push(OffloadRationale {
            field: "accelerator".to_string(),
            explanation: format!(
                "The required {required:?} accelerator is unavailable on this machine; falling back to CPU-only execution."
            ),
        });
        improvements.push(format!(
            "This model requires {required:?}; install or enable a compatible driver, or choose a CPU-friendly variant."
        ));
        return AcceleratorKind::Cpu;
    }
    if profile.preferred_accelerator != AcceleratorKind::Cpu
        && hardware.platform.supports_accelerator(profile.preferred_accelerator)
    {
        rationale.push(OffloadRationale {
            field: "accelerator".to_string(),
            explanation: format!(
                "Opportunistically offloading to {:?}, the preferred accelerator detected on this machine.",
                profile.preferred_accelerator
            ),
        });
        return profile.preferred_accelerator;
    }
    rationale.push(OffloadRationale {
        field: "accelerator".to_string(),
        explanation: "No GPU or Metal accelerator is available; the plan runs entirely on CPU.".to_string(),
    });
    AcceleratorKind::Cpu
}

/// Coarse dense-transformer layer-count estimate derived from on-disk weight
/// size. GGUF/safetensors metadata (the real source of truth for layer
/// count) is not read at planning time, so this only needs the right order
/// of magnitude: it turns a VRAM-fit fraction into a friendly "N of M layers
/// offloaded" count instead of a raw percentage.
fn estimate_layer_count(weights_bytes: u64) -> u32 {
    const GIB: u64 = 1024 * 1024 * 1024;
    if weights_bytes < 2 * GIB {
        24
    } else if weights_bytes < 5 * GIB {
        28
    } else if weights_bytes < 10 * GIB {
        32
    } else if weights_bytes < 18 * GIB {
        40
    } else if weights_bytes < 40 * GIB {
        48
    } else if weights_bytes < 80 * GIB {
        64
    } else if weights_bytes < 150 * GIB {
        80
    } else {
        96
    }
}

/// Estimates KV-cache growth per token by treating the gap between a
/// model's baseline footprint (weights + KV/overhead at
/// `OFFLOAD_BASELINE_CONTEXT_TOKENS`) and its exact on-disk weight size as
/// the KV/overhead budget for that baseline context.
fn estimate_kv_bytes_per_token(model: &OffloadModelProfile) -> u64 {
    let baseline_footprint = model.estimated_vram_bytes.max(model.estimated_ram_bytes);
    let kv_baseline = baseline_footprint.saturating_sub(model.weights_bytes);
    if kv_baseline == 0 {
        return OFFLOAD_MIN_KV_BYTES_PER_TOKEN;
    }
    (kv_baseline / u64::from(OFFLOAD_BASELINE_CONTEXT_TOKENS)).max(OFFLOAD_MIN_KV_BYTES_PER_TOKEN)
}

/// Snaps a raw token count down to the nearest common context tier so the
/// plan reads like a runtime flag (2048, 4096, 8192, ...) instead of an
/// arbitrary computed number.
fn pick_context_tier(value: u32) -> u32 {
    OFFLOAD_CONTEXT_TIERS
        .iter()
        .rev()
        .find(|&&tier| tier <= value)
        .copied()
        .unwrap_or(OFFLOAD_MIN_CONTEXT_TOKENS)
}

fn format_bytes_for_rationale(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{value:.0} {}", UNITS[unit_index])
    } else {
        format!("{value:.1} {}", UNITS[unit_index])
    }
}

fn validate_offload_plan_input(input: &OffloadPlanInput) -> RuntimeAdapterResult<()> {
    if input.model.weights_bytes == 0 {
        return Err(RuntimeAdapterError::InvalidOperationLimits {
            message: "offload plan model weights must be a positive byte count".to_string(),
        });
    }
    if input.reserved.ram_bytes > input.hardware.total_ram_bytes {
        return Err(RuntimeAdapterError::InvalidOperationLimits {
            message: "offload plan reserved RAM cannot exceed total system RAM".to_string(),
        });
    }
    if let Some(context) = input.requested_context_tokens {
        if context == 0 {
            return Err(RuntimeAdapterError::InvalidOperationLimits {
                message: "offload plan requested context tokens must be positive".to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    /// An absolute path valid on whichever OS this actually runs under.
    /// `/foo` satisfies `Path::is_absolute()` on Unix but not on Windows
    /// (which requires a drive-letter or UNC prefix) — both the executable
    /// and per-model paths built from this are checked with exactly that,
    /// and never touch real disk I/O, so any platform-appropriate absolute
    /// path is equally valid here.
    fn fixture_absolute_path(rest: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", rest.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/{rest}"))
        }
    }

    /// Same idea as [`fixture_absolute_path`], but rendered as the `String`
    /// that ends up on the launched process's argv (see
    /// `ManagedLlamaCppAdapter::load_model`, which turns a model's
    /// `local_path` into a `-m`/`--model-draft` argument via
    /// `Path::to_str`). Real llama-server invocations legitimately need the
    /// OS-native separator here — unlike `tools.rs`'s glob/grep results,
    /// this string is consumed by an actual subprocess's file open, not
    /// shown to the model/UI — so tests must compare against this rendering
    /// rather than a hardcoded forward-slash literal.
    fn fixture_absolute_path_arg(rest: &str) -> String {
        fixture_absolute_path(rest).to_string_lossy().into_owned()
    }

    const ALPHA_MODEL_PATH: &str = "models/alpha.gguf";
    const BETA_MODEL_PATH: &str = "models/beta.gguf";

    #[derive(Clone)]
    enum TransportPlan {
        Response(HttpResponse),
        Delay(Duration, HttpResponse),
    }

    #[derive(Default)]
    struct MockTransport {
        plans: Mutex<VecDeque<TransportPlan>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn push_json(&self, status: u16, value: Value) {
            self.push_response(
                status,
                serde_json::to_vec(&value).expect("serialize mock response"),
            );
        }

        fn push_response(&self, status: u16, body: Vec<u8>) {
            self.plans
                .lock()
                .expect("lock mock plans")
                .push_back(TransportPlan::Response(HttpResponse { status, body }));
        }

        fn push_delayed_json(&self, delay: Duration, status: u16, value: Value) {
            self.plans
                .lock()
                .expect("lock mock plans")
                .push_back(TransportPlan::Delay(
                    delay,
                    HttpResponse {
                        status,
                        body: serde_json::to_vec(&value).expect("serialize delayed response"),
                    },
                ));
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().expect("lock requests").clone()
        }
    }

    impl HttpTransport for MockTransport {
        fn execute<'a>(
            &'a self,
            request: HttpRequest,
            _cancellation: &'a CancellationToken,
        ) -> RuntimeFuture<'a, HttpResponse> {
            self.requests.lock().expect("lock requests").push(request);
            let plan = self
                .plans
                .lock()
                .expect("lock mock plans")
                .pop_front()
                .unwrap_or_else(|| {
                    TransportPlan::Response(HttpResponse {
                        status: 599,
                        body: b"unexpected mock request".to_vec(),
                    })
                });
            Box::pin(async move {
                match plan {
                    TransportPlan::Response(response) => Ok(response),
                    TransportPlan::Delay(delay, response) => {
                        tokio::time::sleep(delay).await;
                        Ok(response)
                    }
                }
            })
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ControllerCall {
        Port(u16),
        Launch(ManagedProcessSpec),
        Inspect(String),
        Terminate(String),
        Logs(String, usize),
    }

    #[derive(Default)]
    struct MockController {
        port_results: Mutex<VecDeque<RuntimeAdapterResult<Option<PortOwnership>>>>,
        launch_results: Mutex<VecDeque<RuntimeAdapterResult<ManagedProcessHandle>>>,
        inspect_results: Mutex<VecDeque<RuntimeAdapterResult<ManagedProcessStatus>>>,
        terminate_results: Mutex<VecDeque<RuntimeAdapterResult<()>>>,
        log_results: Mutex<VecDeque<RuntimeAdapterResult<ManagedLogChunk>>>,
        calls: Mutex<Vec<ControllerCall>>,
    }

    impl MockController {
        fn push_port(&self, result: RuntimeAdapterResult<Option<PortOwnership>>) {
            self.port_results
                .lock()
                .expect("lock ports")
                .push_back(result);
        }

        fn push_launch(&self, result: RuntimeAdapterResult<ManagedProcessHandle>) {
            self.launch_results
                .lock()
                .expect("lock launches")
                .push_back(result);
        }

        fn push_inspect(&self, result: RuntimeAdapterResult<ManagedProcessStatus>) {
            self.inspect_results
                .lock()
                .expect("lock inspections")
                .push_back(result);
        }

        fn push_logs(&self, result: RuntimeAdapterResult<ManagedLogChunk>) {
            self.log_results
                .lock()
                .expect("lock logs")
                .push_back(result);
        }

        fn calls(&self) -> Vec<ControllerCall> {
            self.calls.lock().expect("lock calls").clone()
        }
    }

    impl ManagedProcessController for MockController {
        fn port_owner<'a>(
            &'a self,
            port: u16,
            _context: &'a RuntimeOperationContext,
        ) -> RuntimeFuture<'a, Option<PortOwnership>> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(ControllerCall::Port(port));
            let result = self
                .port_results
                .lock()
                .expect("lock ports")
                .pop_front()
                .unwrap_or(Ok(None));
            Box::pin(async move { result })
        }

        fn launch<'a>(
            &'a self,
            spec: ManagedProcessSpec,
            _context: &'a RuntimeOperationContext,
        ) -> RuntimeFuture<'a, ManagedProcessHandle> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(ControllerCall::Launch(spec.clone()));
            let result = self
                .launch_results
                .lock()
                .expect("lock launches")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(RuntimeAdapterError::Controller {
                        operation: "mock launch".to_string(),
                        message: "missing launch plan".to_string(),
                    })
                });
            Box::pin(async move { result })
        }

        fn inspect<'a>(
            &'a self,
            handle: &'a ManagedProcessHandle,
            _context: &'a RuntimeOperationContext,
        ) -> RuntimeFuture<'a, ManagedProcessStatus> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(ControllerCall::Inspect(handle.process_id.clone()));
            let default = ManagedProcessStatus {
                handle: handle.clone(),
                state: ManagedProcessState::Ready,
                exit_code: None,
                message: None,
            };
            let result = self
                .inspect_results
                .lock()
                .expect("lock inspections")
                .pop_front()
                .unwrap_or(Ok(default));
            Box::pin(async move { result })
        }

        fn terminate<'a>(
            &'a self,
            handle: &'a ManagedProcessHandle,
            _context: &'a RuntimeOperationContext,
        ) -> RuntimeFuture<'a, ()> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(ControllerCall::Terminate(handle.process_id.clone()));
            let result = self
                .terminate_results
                .lock()
                .expect("lock terminations")
                .pop_front()
                .unwrap_or(Ok(()));
            Box::pin(async move { result })
        }

        fn tail_logs<'a>(
            &'a self,
            handle: &'a ManagedProcessHandle,
            max_bytes: usize,
            _context: &'a RuntimeOperationContext,
        ) -> RuntimeFuture<'a, ManagedLogChunk> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(ControllerCall::Logs(handle.process_id.clone(), max_bytes));
            let result = self
                .log_results
                .lock()
                .expect("lock logs")
                .pop_front()
                .unwrap_or(Ok(ManagedLogChunk {
                    text: String::new(),
                    truncated: false,
                }));
            Box::pin(async move { result })
        }
    }

    fn cpu_capability() -> AcceleratorCapability {
        AcceleratorCapability {
            kind: AcceleratorKind::Cpu,
            available: true,
            device_names: vec!["CPU".to_string()],
            total_memory_bytes: None,
            available_memory_bytes: None,
        }
    }

    fn platform() -> PlatformCapabilities {
        PlatformCapabilities::from_host("linux", "x86_64", vec![cpu_capability()])
    }

    fn context() -> RuntimeOperationContext {
        RuntimeOperationContext::default()
    }

    fn ollama(transport: Arc<MockTransport>) -> OllamaHttpAdapter {
        OllamaHttpAdapter::new(
            "ollama-main",
            "http://127.0.0.1:11434",
            EndpointPolicy::LoopbackOnly,
            transport,
            platform(),
        )
        .expect("create Ollama adapter")
    }

    fn model(model_id: &str, path: &str) -> RuntimeModel {
        RuntimeModel {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
            size_bytes: 8 * 1024 * 1024,
            local_path: Some(fixture_absolute_path(path)),
            digest: Some(format!("digest-{model_id}")),
            modified_at: None,
            capabilities: ModelCapabilities {
                chat: true,
                embeddings: false,
                tool_calling: true,
                vision: false,
            },
            metadata: BTreeMap::new(),
        }
    }

    fn process_handle() -> ManagedProcessHandle {
        ManagedProcessHandle {
            process_id: "process-1".to_string(),
            os_pid: Some(42),
            port: 8090,
            started_at_ms: 123,
        }
    }

    fn llama(controller: Arc<MockController>) -> ManagedLlamaCppAdapter {
        ManagedLlamaCppAdapter::new(
            "llama-chat",
            "http://127.0.0.1:8090",
            fixture_absolute_path("usr/local/bin/llama-server"),
            8090,
            controller,
            vec![
                model("alpha", ALPHA_MODEL_PATH),
                model("beta", BETA_MODEL_PATH),
            ],
            platform(),
        )
        .expect("create llama.cpp adapter")
    }

    fn load_request(model_id: &str) -> ModelLoadRequest {
        ModelLoadRequest {
            model_id: model_id.to_string(),
            keep_alive: None,
            settings: BTreeMap::new(),
            replace_existing: false,
        }
    }

    #[test]
    fn endpoint_origins_and_platform_matrix_are_strict_and_serializable() {
        let local = EndpointOrigin::parse("http://LOCALHOST:11434/", EndpointPolicy::LoopbackOnly)
            .expect("local endpoint");
        assert_eq!(local.as_str(), "http://localhost:11434");
        assert!(local.is_loopback());
        assert_eq!(
            local.url("/api/tags").expect("API URL"),
            "http://localhost:11434/api/tags"
        );
        let round_trip: EndpointOrigin =
            serde_json::from_slice(&serde_json::to_vec(&local).expect("serialize endpoint origin"))
                .expect("validated endpoint round trip");
        assert_eq!(round_trip, local);
        assert!(serde_json::from_value::<EndpointOrigin>(json!({
            "origin": "file:///tmp/socket",
            "loopback": false
        }))
        .is_err());
        assert!(serde_json::from_value::<EndpointOrigin>(json!({
            "origin": "https://runtime.example",
            "loopback": true
        }))
        .is_err());

        for invalid in [
            "ftp://localhost:11434",
            "http://user:secret@localhost:11434",
            "http://localhost:11434/api",
            "http://localhost:11434/?query=yes",
            "http://192.168.1.9:11434",
        ] {
            assert!(matches!(
                EndpointOrigin::parse(invalid, EndpointPolicy::LoopbackOnly),
                Err(RuntimeAdapterError::InvalidEndpoint { .. })
            ));
        }
        assert!(EndpointOrigin::parse(
            "http://runtime.example:11434",
            EndpointPolicy::AllowRemoteHttps
        )
        .is_err());
        assert!(
            EndpointOrigin::parse("https://runtime.example", EndpointPolicy::AllowRemoteHttps)
                .is_ok()
        );

        let mac = PlatformCapabilities::from_host(
            "darwin",
            "arm64",
            vec![AcceleratorCapability {
                kind: AcceleratorKind::Metal,
                available: true,
                device_names: vec!["Apple GPU".to_string()],
                total_memory_bytes: None,
                available_memory_bytes: None,
            }],
        );
        assert_eq!(mac.os, "macos");
        assert_eq!(mac.arch, "aarch64");
        assert!(mac.supports_runtime(RuntimeKind::LlamaCpp));
        assert!(mac.supports_accelerator(AcceleratorKind::Metal));

        let windows = PlatformCapabilities::from_host(
            "win32",
            "amd64",
            vec![AcceleratorCapability {
                kind: AcceleratorKind::DirectMl,
                available: true,
                device_names: Vec::new(),
                total_memory_bytes: None,
                available_memory_bytes: None,
            }],
        );
        assert!(windows.supports_accelerator(AcceleratorKind::DirectMl));
        assert!(!windows.supports_accelerator(AcceleratorKind::Metal));

        let unknown = PlatformCapabilities::from_host("plan9", "mips", vec![]);
        assert!(unknown.supported_runtimes.is_empty());
        assert!(!unknown.supports_accelerator(AcceleratorKind::Cpu));

        let snapshot = HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 64 * 1024 * 1024 * 1024,
            available_ram_bytes: 48 * 1024 * 1024 * 1024,
            logical_cpu_count: 16,
            platform: mac,
        };
        let profile = snapshot.profile().expect("hardware profile");
        assert_eq!(profile.tier, HardwareTier::Performance);
        assert_eq!(profile.recommended_process_slots, 4);
        assert_eq!(profile.preferred_accelerator, AcceleratorKind::Metal);
        serde_json::to_vec(&snapshot).expect("serialize hardware snapshot");
    }

    #[tokio::test]
    async fn ollama_status_inventory_running_load_keepalive_and_exact_unload_work() {
        let transport = Arc::new(MockTransport::default());
        let adapter = ollama(transport.clone());
        let context = context();

        transport.push_json(200, json!({"version": "0.12.0"}));
        let status = adapter.status(&context).await.expect("Ollama status");
        assert_eq!(status.state, RuntimeLifecycleState::Ready);
        assert_eq!(status.version.as_deref(), Some("0.12.0"));

        transport.push_json(
            200,
            json!({"models": [
                {"name": "zeta:latest", "size": 9, "digest": "z", "modified_at": "today"},
                {"model": "alpha:latest", "size": 7, "digest": "a"}
            ]}),
        );
        let inventory = adapter.inventory(&context).await.expect("model inventory");
        assert_eq!(
            inventory
                .models
                .iter()
                .map(|entry| entry.model_id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha:latest", "zeta:latest"]
        );

        transport.push_json(
            200,
            json!({"models": [{
                "name": "preexisting:latest", "size": 100, "size_vram": 80,
                "digest": "p", "expires_at": "later"
            }]}),
        );
        let running = adapter
            .running_models(&context)
            .await
            .expect("running models");
        assert_eq!(running[0].ownership, ResidencyOwnership::PreExisting);

        transport.push_json(200, json!({"models": []}));
        transport.push_json(200, json!({"done": true}));
        let mut request = load_request("alpha:latest");
        request.keep_alive = Some(KeepAlive::DurationMs {
            milliseconds: 9_000,
        });
        request.settings.insert(
            "num_ctx".to_string(),
            SettingValue::Integer { value: 8_192 },
        );
        let loaded = adapter
            .load_model(&request, &context)
            .await
            .expect("load Ollama model");
        assert_eq!(loaded.disposition, ModelLoadDisposition::Loaded);
        assert_eq!(loaded.ownership, ResidencyOwnership::AppManaged);

        transport.push_json(
            200,
            json!({"models": [{"name": "alpha:latest", "size": 100}]}),
        );
        transport.push_json(200, json!({"done": true}));
        adapter
            .set_keep_alive(
                &KeepAliveRequest {
                    model_id: "alpha:latest".to_string(),
                    keep_alive: KeepAlive::Forever,
                },
                &context,
            )
            .await
            .expect("refresh keep alive");

        transport.push_json(
            200,
            json!({"models": [
                {"name": "alpha:latest", "size": 100},
                {"name": "alpha:latest-extra", "size": 100}
            ]}),
        );
        transport.push_json(200, json!({"done": true}));
        let unloaded = adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "alpha:latest".to_string(),
                    policy: UnloadPolicy::AppManagedOnly,
                },
                &context,
            )
            .await
            .expect("unload exact owned model");
        assert_eq!(unloaded.disposition, ModelUnloadDisposition::Unloaded);

        transport.push_json(
            200,
            json!({"models": [{"name": "alpha:latest", "size": 100}]}),
        );
        let mismatch = adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "ALPHA:latest".to_string(),
                    policy: UnloadPolicy::ExactRegardlessOfOwner,
                },
                &context,
            )
            .await
            .expect("case-sensitive exact preflight");
        assert_eq!(mismatch.disposition, ModelUnloadDisposition::NotRunning);

        transport.push_json(
            200,
            json!({"models": [{"name": "alpha:latest", "size": 100}]}),
        );
        let preserved = adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "alpha:latest".to_string(),
                    policy: UnloadPolicy::AppManagedOnly,
                },
                &context,
            )
            .await
            .expect("preserve pre-existing residency");
        assert_eq!(
            preserved.disposition,
            ModelUnloadDisposition::PreservedPreExisting
        );

        transport.push_json(
            200,
            json!({"models": [{"name": "alpha:latest", "size": 100}]}),
        );
        transport.push_json(200, json!({"done": true}));
        adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "alpha:latest".to_string(),
                    policy: UnloadPolicy::ExactRegardlessOfOwner,
                },
                &context,
            )
            .await
            .expect("explicit exact external unload");

        let requests = transport.requests();
        let post_bodies: Vec<Value> = requests
            .iter()
            .filter(|request| request.method == HttpMethod::Post)
            .map(|request| {
                serde_json::from_slice(request.body.as_ref().expect("POST body"))
                    .expect("parse POST body")
            })
            .collect();
        assert_eq!(post_bodies[0]["model"], "alpha:latest");
        assert_eq!(post_bodies[0]["keep_alive"], "9000ms");
        assert_eq!(post_bodies[0]["options"]["num_ctx"], 8_192);
        assert_eq!(post_bodies[2]["model"], "alpha:latest");
        assert_eq!(post_bodies[2]["keep_alive"], 0);
        assert_eq!(post_bodies.last().expect("last POST")["keep_alive"], 0);
    }

    #[tokio::test]
    async fn unsupported_settings_fail_before_transport_or_controller_side_effects() {
        let transport = Arc::new(MockTransport::default());
        let adapter = ollama(transport.clone());
        let mut request = load_request("alpha:latest");
        request.settings.insert(
            "made_up_flag".to_string(),
            SettingValue::Boolean { value: true },
        );
        let error = adapter
            .load_model(&request, &context())
            .await
            .expect_err("unsupported Ollama setting");
        assert!(matches!(
            error,
            RuntimeAdapterError::UnsupportedSetting { .. }
        ));
        assert!(transport.requests().is_empty());

        let config_transport = Arc::new(MockTransport::default());
        let config_adapter = ollama(config_transport.clone());
        let mut supported = load_request("alpha:latest");
        supported.settings.insert(
            "num_ctx".to_string(),
            SettingValue::Integer { value: 8_192 },
        );
        let config_context = RuntimeOperationContext::new(
            RuntimeOperationLimits {
                max_config_bytes: 16,
                ..RuntimeOperationLimits::default()
            },
            CancellationToken::new(),
        );
        assert!(matches!(
            config_adapter.load_model(&supported, &config_context).await,
            Err(RuntimeAdapterError::ConfigTooLarge { limit: 16, .. })
        ));
        assert!(config_transport.requests().is_empty());

        let controller = Arc::new(MockController::default());
        let adapter = llama(controller.clone());
        let error = adapter
            .load_model(&request, &context())
            .await
            .expect_err("unsupported llama.cpp setting");
        assert!(matches!(
            error,
            RuntimeAdapterError::UnsupportedSetting { .. }
        ));
        assert!(controller.calls().is_empty());
    }

    #[tokio::test]
    async fn malformed_oversized_cancelled_and_timed_out_http_operations_fail_boundedly() {
        let malformed_transport = Arc::new(MockTransport::default());
        malformed_transport.push_response(200, b"not-json".to_vec());
        let malformed = ollama(malformed_transport);
        assert!(matches!(
            malformed.status(&context()).await,
            Err(RuntimeAdapterError::MalformedResponse { .. })
        ));

        let oversized_transport = Arc::new(MockTransport::default());
        oversized_transport.push_response(200, vec![b'x'; 33]);
        let oversized = ollama(oversized_transport);
        let limits = RuntimeOperationLimits {
            max_response_bytes: 32,
            ..RuntimeOperationLimits::default()
        };
        let bounded_context = RuntimeOperationContext::new(limits, CancellationToken::new());
        assert!(matches!(
            oversized.status(&bounded_context).await,
            Err(RuntimeAdapterError::ResponseTooLarge {
                limit: 32,
                actual_at_least: 33
            })
        ));

        let cancelled_transport = Arc::new(MockTransport::default());
        cancelled_transport.push_delayed_json(
            Duration::from_millis(200),
            200,
            json!({"version": "late"}),
        );
        let cancelled = ollama(cancelled_transport);
        let cancellation = CancellationToken::new();
        let cancel_from_task = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_from_task.cancel();
        });
        let cancelled_context =
            RuntimeOperationContext::new(RuntimeOperationLimits::default(), cancellation);
        assert!(matches!(
            cancelled.status(&cancelled_context).await,
            Err(RuntimeAdapterError::Cancelled { .. })
        ));

        let timeout_transport = Arc::new(MockTransport::default());
        timeout_transport.push_delayed_json(
            Duration::from_millis(50),
            200,
            json!({"version": "late"}),
        );
        let timeout = ollama(timeout_transport);
        let limits = RuntimeOperationLimits {
            timeout_ms: 5,
            ..RuntimeOperationLimits::default()
        };
        let timeout_context = RuntimeOperationContext::new(limits, CancellationToken::new());
        assert!(matches!(
            timeout.status(&timeout_context).await,
            Err(RuntimeAdapterError::Timeout { timeout_ms: 5, .. })
        ));
    }

    #[tokio::test]
    async fn managed_llama_lifecycle_uses_structured_args_status_logs_and_exact_unload() {
        let controller = Arc::new(MockController::default());
        controller.push_port(Ok(None));
        controller.push_launch(Ok(process_handle()));
        controller.push_inspect(Ok(ManagedProcessStatus {
            handle: process_handle(),
            state: ManagedProcessState::Ready,
            exit_code: None,
            message: Some("healthy".to_string()),
        }));
        let adapter = llama(controller.clone());
        let mut initial = load_request("alpha");
        initial.settings.insert(
            "context_size".to_string(),
            SettingValue::Integer { value: 8_192 },
        );
        initial.settings.insert(
            "gpu_layers".to_string(),
            SettingValue::Integer { value: 32 },
        );
        initial.settings.insert(
            "flash_attention".to_string(),
            SettingValue::Choice {
                value: "on".to_string(),
            },
        );
        let status = adapter
            .start(
                &RuntimeStartRequest {
                    initial_model: Some(initial),
                },
                &context(),
            )
            .await
            .expect("start managed llama.cpp");
        assert_eq!(status.state, RuntimeLifecycleState::Ready);
        assert_eq!(
            status.process.as_ref().and_then(|handle| handle.os_pid),
            Some(42)
        );

        let inventory = adapter.inventory(&context()).await.expect("inventory");
        assert_eq!(inventory.models.len(), 2);
        let running = adapter
            .running_models(&context())
            .await
            .expect("running models");
        assert_eq!(running[0].model_id, "alpha");

        controller.push_logs(Ok(ManagedLogChunk {
            text: "server ready\n".to_string(),
            truncated: false,
        }));
        let logs = adapter
            .tail_logs(&RuntimeLogRequest { max_bytes: 64 }, &context())
            .await
            .expect("tail logs");
        assert_eq!(logs.text, "server ready\n");

        let mismatch = adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "beta".to_string(),
                    policy: UnloadPolicy::ExactRegardlessOfOwner,
                },
                &context(),
            )
            .await
            .expect("exact mismatch is a no-op");
        assert_eq!(mismatch.disposition, ModelUnloadDisposition::NotRunning);
        let unloaded = adapter
            .unload_model(
                &ModelUnloadRequest {
                    model_id: "alpha".to_string(),
                    policy: UnloadPolicy::AppManagedOnly,
                },
                &context(),
            )
            .await
            .expect("unload alpha");
        assert_eq!(unloaded.disposition, ModelUnloadDisposition::Unloaded);
        assert_eq!(
            adapter
                .status(&context())
                .await
                .expect("stopped status")
                .state,
            RuntimeLifecycleState::Stopped
        );

        let calls = controller.calls();
        let spec = calls
            .iter()
            .find_map(|call| match call {
                ControllerCall::Launch(spec) => Some(spec),
                _ => None,
            })
            .expect("structured launch call");
        assert_eq!(
            spec.program,
            fixture_absolute_path("usr/local/bin/llama-server")
        );
        assert_eq!(
            spec.args[0..2],
            ["-m".to_string(), fixture_absolute_path_arg(ALPHA_MODEL_PATH)]
        );
        assert!(spec.args.windows(2).any(|pair| pair == ["-c", "8192"]));
        assert!(spec.args.windows(2).any(|pair| pair == ["-ngl", "32"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--flash-attn", "on"]));
        assert!(!spec.args.iter().any(|arg| arg.contains("llama-server ")));
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, ControllerCall::Terminate(_)))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn managed_llama_detects_port_collision_and_oversized_logs() {
        let collision_controller = Arc::new(MockController::default());
        collision_controller.push_port(Ok(Some(PortOwnership {
            port: 8090,
            owner_id: "other-service".to_string(),
            runtime: None,
            ownership: ResidencyOwnership::External,
        })));
        let collision_adapter = llama(collision_controller.clone());
        let error = collision_adapter
            .load_model(&load_request("alpha"), &context())
            .await
            .expect_err("port collision");
        assert!(matches!(
            error,
            RuntimeAdapterError::PortCollision {
                port: 8090,
                owner_id
            } if owner_id == "other-service"
        ));
        assert!(!collision_controller
            .calls()
            .iter()
            .any(|call| matches!(call, ControllerCall::Launch(_))));

        let log_controller = Arc::new(MockController::default());
        log_controller.push_launch(Ok(process_handle()));
        let log_adapter = llama(log_controller.clone());
        log_adapter
            .load_model(&load_request("alpha"), &context())
            .await
            .expect("load for log test");
        log_controller.push_logs(Ok(ManagedLogChunk {
            text: "x".repeat(65),
            truncated: false,
        }));
        let error = log_adapter
            .tail_logs(&RuntimeLogRequest { max_bytes: 64 }, &context())
            .await
            .expect_err("controller cannot exceed requested log bound");
        assert!(matches!(
            error,
            RuntimeAdapterError::LogTooLarge {
                limit: 64,
                actual: 65
            }
        ));
    }

    #[tokio::test]
    async fn managed_llama_forwards_sampler_batch_mixed_precision_and_draft_model_args() {
        let controller = Arc::new(MockController::default());
        controller.push_port(Ok(None));
        controller.push_launch(Ok(process_handle()));
        let adapter = llama(controller.clone());
        let mut request = load_request("alpha");
        request
            .settings
            .insert("temperature".to_string(), SettingValue::Float { value: 0.5 });
        request
            .settings
            .insert("top_p".to_string(), SettingValue::Float { value: 0.85 });
        request
            .settings
            .insert("top_k".to_string(), SettingValue::Integer { value: 20 });
        request.settings.insert(
            "repeat_penalty".to_string(),
            SettingValue::Float { value: 1.2 },
        );
        request
            .settings
            .insert("min_p".to_string(), SettingValue::Float { value: 0.02 });
        request
            .settings
            .insert("batch_size".to_string(), SettingValue::Integer { value: 1_024 });
        request.settings.insert(
            "mixed_precision".to_string(),
            SettingValue::Choice {
                value: "q8_0".to_string(),
            },
        );
        // "beta" is a second model already configured on this same adapter
        // (see the `llama()` fixture) — standing in for a smaller,
        // already-installed draft model.
        request.settings.insert(
            "speculative_decoding_draft_model".to_string(),
            SettingValue::Text {
                value: "beta".to_string(),
            },
        );

        adapter
            .load_model(&request, &context())
            .await
            .expect("load with sampler/batch/mixed-precision/draft settings");

        let calls = controller.calls();
        let spec = calls
            .iter()
            .find_map(|call| match call {
                ControllerCall::Launch(spec) => Some(spec),
                _ => None,
            })
            .expect("structured launch call");
        assert!(spec.args.windows(2).any(|pair| pair == ["--temp", "0.5"]));
        assert!(spec.args.windows(2).any(|pair| pair == ["--top-p", "0.85"]));
        assert!(spec.args.windows(2).any(|pair| pair == ["--top-k", "20"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--repeat-penalty", "1.2"]));
        assert!(spec.args.windows(2).any(|pair| pair == ["--min-p", "0.02"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--batch-size", "1024"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--cache-type-k", "q8_0"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--cache-type-v", "q8_0"]));
        let expected_draft_arg = [
            "--model-draft".to_string(),
            fixture_absolute_path_arg(BETA_MODEL_PATH),
        ];
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == expected_draft_arg));
    }

    #[tokio::test]
    async fn managed_llama_rejects_a_draft_model_id_it_does_not_know_about() {
        let controller = Arc::new(MockController::default());
        controller.push_port(Ok(None));
        let adapter = llama(controller);
        let mut request = load_request("alpha");
        request.settings.insert(
            "speculative_decoding_draft_model".to_string(),
            SettingValue::Text {
                value: "not-a-configured-model".to_string(),
            },
        );

        let error = adapter
            .load_model(&request, &context())
            .await
            .expect_err("unknown draft model id must fail before launch");
        assert!(matches!(
            error,
            RuntimeAdapterError::ModelNotFound { model_id, .. } if model_id == "not-a-configured-model"
        ));
    }

    #[test]
    fn llama_setting_capabilities_gate_flash_attention_true_and_draft_model_false_by_default() {
        let capabilities = llama_setting_capabilities();
        let flash_attention = capabilities
            .iter()
            .find(|capability| capability.key == "flash_attention")
            .expect("flash_attention capability declared");
        assert!(flash_attention.supported);
        assert!(flash_attention.unsupported_reason.is_none());

        let mixed_precision = capabilities
            .iter()
            .find(|capability| capability.key == "mixed_precision")
            .expect("mixed_precision capability declared");
        assert!(mixed_precision.supported);

        // Speculative decoding is relative to a target model this adapter
        // has no notion of, so its baseline is disabled with a reason —
        // never a silently no-op enabled control. The Runtime Hub layer
        // (`m3_runtime_hub.rs`'s `gate_advanced_settings`) is what flips
        // this once a compatible target/draft pair is known.
        let draft_model = capabilities
            .iter()
            .find(|capability| capability.key == "speculative_decoding_draft_model")
            .expect("speculative_decoding_draft_model capability declared");
        assert!(!draft_model.supported);
        assert!(draft_model.unsupported_reason.is_some());
    }

    #[test]
    fn ollama_setting_capabilities_expose_sampler_and_batch_controls_unconditionally() {
        let capabilities = ollama_setting_capabilities();
        for key in ["temperature", "top_p", "top_k", "repeat_penalty", "min_p", "num_batch"] {
            let capability = capabilities
                .iter()
                .find(|capability| capability.key == key)
                .unwrap_or_else(|| panic!("{key} capability declared"));
            assert!(capability.supported, "{key} should be unconditionally supported");
            assert!(capability.unsupported_reason.is_none());
        }
    }

    #[test]
    fn scheduler_queues_memory_and_slot_conflicts_while_preserving_existing_residency() {
        let input = SchedulingInput {
            platform: platform(),
            memory: MemoryBudget {
                available_ram_bytes: 18,
                reserve_ram_bytes: 2,
                available_vram_bytes: 0,
                reserve_vram_bytes: 0,
            },
            process_slots: vec![
                ProcessSlot {
                    slot_id: "slot-a".to_string(),
                    runtime: RuntimeKind::LlamaCpp,
                    port: Some(8090),
                    state: ProcessSlotState::Available,
                },
                ProcessSlot {
                    slot_id: "slot-b".to_string(),
                    runtime: RuntimeKind::LlamaCpp,
                    port: Some(8091),
                    state: ProcessSlotState::Available,
                },
            ],
            residents: vec![ResidentModelAllocation {
                runtime: RuntimeKind::Ollama,
                model_id: "resident".to_string(),
                memory: MemoryRequirement {
                    ram_bytes: 7,
                    vram_bytes: 0,
                },
                ownership: ResidencyOwnership::PreExisting,
                slot_id: None,
                port: None,
            }],
            ports: Vec::new(),
            targets: vec![
                ScheduleTarget {
                    target_id: "reuse".to_string(),
                    runtime: RuntimeKind::Ollama,
                    model_id: "resident".to_string(),
                    memory: MemoryRequirement {
                        ram_bytes: 7,
                        vram_bytes: 0,
                    },
                    accelerator: Some(AcceleratorKind::Cpu),
                    preferred_slot_id: None,
                },
                ScheduleTarget {
                    target_id: "branch-a".to_string(),
                    runtime: RuntimeKind::LlamaCpp,
                    model_id: "alpha".to_string(),
                    memory: MemoryRequirement {
                        ram_bytes: 10,
                        vram_bytes: 0,
                    },
                    accelerator: Some(AcceleratorKind::Cpu),
                    preferred_slot_id: None,
                },
                ScheduleTarget {
                    target_id: "branch-b".to_string(),
                    runtime: RuntimeKind::LlamaCpp,
                    model_id: "beta".to_string(),
                    memory: MemoryRequirement {
                        ram_bytes: 10,
                        vram_bytes: 0,
                    },
                    accelerator: Some(AcceleratorKind::Cpu),
                    preferred_slot_id: None,
                },
            ],
        };
        let plan = LocalRuntimeScheduler::plan(&input).expect("build schedule");
        assert_eq!(plan.waves.len(), 2);
        assert_eq!(plan.preserved_residency, input.residents);
        let reused = plan.waves[0]
            .targets
            .iter()
            .find(|target| target.target_id == "reuse")
            .expect("reused target");
        assert_eq!(reused.residency, ScheduledResidency::ReuseExisting);
        assert_eq!(reused.cleanup, ScheduledCleanup::Preserve);
        let branch_b = plan
            .waves
            .iter()
            .flat_map(|wave| &wave.targets)
            .find(|target| target.target_id == "branch-b")
            .expect("branch b");
        assert!(branch_b.queued);
        assert_eq!(branch_b.cleanup, ScheduledCleanup::UnloadAppManaged);

        let one_slot_input = SchedulingInput {
            memory: MemoryBudget {
                available_ram_bytes: 100,
                reserve_ram_bytes: 0,
                ..input.memory.clone()
            },
            process_slots: vec![input.process_slots[0].clone()],
            residents: Vec::new(),
            targets: input.targets[1..].to_vec(),
            ..input.clone()
        };
        let one_slot = LocalRuntimeScheduler::plan(&one_slot_input).expect("queue one slot");
        assert_eq!(one_slot.waves.len(), 2);
    }

    #[test]
    fn scheduler_rejects_port_collision_memory_and_platform_incompatibility() {
        let base = SchedulingInput {
            platform: platform(),
            memory: MemoryBudget {
                available_ram_bytes: 16,
                reserve_ram_bytes: 0,
                available_vram_bytes: 0,
                reserve_vram_bytes: 0,
            },
            process_slots: vec![ProcessSlot {
                slot_id: "slot".to_string(),
                runtime: RuntimeKind::LlamaCpp,
                port: Some(8090),
                state: ProcessSlotState::Available,
            }],
            residents: Vec::new(),
            ports: vec![PortOwnership {
                port: 8090,
                owner_id: "other".to_string(),
                runtime: None,
                ownership: ResidencyOwnership::External,
            }],
            targets: vec![ScheduleTarget {
                target_id: "branch".to_string(),
                runtime: RuntimeKind::LlamaCpp,
                model_id: "alpha".to_string(),
                memory: MemoryRequirement {
                    ram_bytes: 8,
                    vram_bytes: 0,
                },
                accelerator: Some(AcceleratorKind::Cpu),
                preferred_slot_id: None,
            }],
        };
        assert!(matches!(
            LocalRuntimeScheduler::plan(&base),
            Err(RuntimeAdapterError::PortCollision { port: 8090, .. })
        ));

        let no_memory = SchedulingInput {
            ports: Vec::new(),
            targets: vec![ScheduleTarget {
                memory: MemoryRequirement {
                    ram_bytes: 17,
                    vram_bytes: 0,
                },
                ..base.targets[0].clone()
            }],
            ..base.clone()
        };
        assert!(matches!(
            LocalRuntimeScheduler::plan(&no_memory),
            Err(RuntimeAdapterError::InsufficientMemory { .. })
        ));

        let no_accelerator = SchedulingInput {
            ports: Vec::new(),
            targets: vec![ScheduleTarget {
                accelerator: Some(AcceleratorKind::Cuda),
                ..base.targets[0].clone()
            }],
            ..base
        };
        assert!(matches!(
            LocalRuntimeScheduler::plan(&no_accelerator),
            Err(RuntimeAdapterError::IncompatiblePlatform {
                accelerator: AcceleratorKind::Cuda,
                ..
            })
        ));
    }

    fn gib(count: u64) -> u64 {
        count * 1024 * 1024 * 1024
    }

    fn cpu_only_hardware(total_ram_gib: u64, available_ram_gib: u64, cpu_count: u32) -> HardwareSnapshot {
        HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: gib(total_ram_gib),
            available_ram_bytes: gib(available_ram_gib),
            logical_cpu_count: cpu_count,
            platform: PlatformCapabilities::from_host("linux", "x86_64", vec![cpu_capability()]),
        }
    }

    fn metal_hardware(total_ram_gib: u64, available_ram_gib: u64, cpu_count: u32) -> HardwareSnapshot {
        HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: gib(total_ram_gib),
            available_ram_bytes: gib(available_ram_gib),
            logical_cpu_count: cpu_count,
            platform: PlatformCapabilities::from_host(
                "macos",
                "aarch64",
                vec![
                    cpu_capability(),
                    AcceleratorCapability {
                        kind: AcceleratorKind::Metal,
                        available: true,
                        device_names: vec!["Apple Silicon unified GPU".to_string()],
                        total_memory_bytes: Some(gib(total_ram_gib)),
                        available_memory_bytes: Some(gib(available_ram_gib)),
                    },
                ],
            ),
        }
    }

    fn zero_reserved() -> MemoryRequirement {
        MemoryRequirement {
            ram_bytes: 0,
            vram_bytes: 0,
        }
    }

    #[test]
    fn offload_plan_cpu_only_balanced_tier_matches_hand_computed_values() {
        let hardware = cpu_only_hardware(16, 10, 8);
        let input = OffloadPlanInput {
            hardware,
            model: OffloadModelProfile {
                weights_bytes: gib(4),
                estimated_ram_bytes: gib(4) + gib(1) / 2,
                estimated_vram_bytes: 0,
                required_accelerator: None,
                has_vision_projector: false,
                projector_memory_bytes: 0,
            },
            reserved: zero_reserved(),
            other_resident_count: 0,
            requested_context_tokens: None,
        };
        let plan = LocalOffloadPlanner::plan(&input).expect("cpu-only plan");

        assert_eq!(plan.accelerator, AcceleratorKind::Cpu);
        assert_eq!(plan.estimated_total_layers, 28);
        assert_eq!(plan.gpu_layers, 0);
        assert_eq!(plan.cpu_spill_layers, 28);
        assert_eq!(plan.context_tokens, 8_192);
        assert_eq!(plan.requested_context_tokens, 8_192);
        assert_eq!(plan.batch_size, 256);
        assert_eq!(plan.parallel_sequences, 2);
        assert_eq!(plan.projector_placement, ProjectorPlacement::NotApplicable);
        assert_eq!(plan.available_ram_bytes, gib(7));
        assert_eq!(plan.available_vram_bytes, 0);
        assert_eq!(plan.rationale.len(), 5);
        assert_eq!(plan.improvement_suggestions.len(), 1);
        assert!(plan.improvement_suggestions[0].contains("No GPU or Metal acceleration"));
    }

    #[test]
    fn offload_plan_metal_partial_offload_reduces_context_batch_and_parallelism() {
        let hardware = metal_hardware(40, 30, 12);
        let input = OffloadPlanInput {
            hardware,
            model: OffloadModelProfile {
                weights_bytes: gib(4),
                estimated_ram_bytes: gib(4) + gib(1) / 2,
                estimated_vram_bytes: gib(4) + gib(1) / 2,
                required_accelerator: None,
                has_vision_projector: true,
                projector_memory_bytes: 0,
            },
            reserved: MemoryRequirement {
                ram_bytes: gib(22),
                vram_bytes: 0,
            },
            other_resident_count: 2,
            requested_context_tokens: None,
        };
        let plan = LocalOffloadPlanner::plan(&input).expect("metal plan");

        assert_eq!(plan.accelerator, AcceleratorKind::Metal);
        assert_eq!(plan.estimated_total_layers, 28);
        assert_eq!(plan.gpu_layers, 24);
        assert_eq!(plan.cpu_spill_layers, 4);
        assert_eq!(plan.context_tokens, 512);
        assert_eq!(plan.requested_context_tokens, 8_192);
        assert_eq!(plan.batch_size, 256);
        assert_eq!(plan.parallel_sequences, 1);
        assert_eq!(plan.projector_placement, ProjectorPlacement::Gpu);
        assert_eq!(plan.available_ram_bytes, gib(4));
        assert_eq!(plan.available_vram_bytes, gib(4));
        assert_eq!(plan.rationale.len(), 6);
        assert_eq!(plan.improvement_suggestions.len(), 2);
        assert!(plan.improvement_suggestions[0].contains("Unload 2 other resident model"));
        assert!(plan.improvement_suggestions[1].contains("capped at 512"));
    }

    #[test]
    fn offload_plan_reserves_projector_memory_before_sizing_context_and_gpu_layers() {
        // Same hardware/model shape as the metal-partial-offload case above,
        // except this model's projector itself needs 2 GiB of resident
        // memory. That must come off the top of the same unified pool
        // *before* GPU-layer fit and context-tier math, producing a smaller
        // affordable context/available-memory than the zero-projector-memory
        // case, plus an explicit rationale entry naming the reservation.
        let hardware = metal_hardware(40, 30, 12);
        let input = OffloadPlanInput {
            hardware,
            model: OffloadModelProfile {
                weights_bytes: gib(4),
                estimated_ram_bytes: gib(4) + gib(1) / 2,
                estimated_vram_bytes: gib(4) + gib(1) / 2,
                required_accelerator: None,
                has_vision_projector: true,
                projector_memory_bytes: gib(2),
            },
            reserved: MemoryRequirement {
                ram_bytes: gib(22),
                vram_bytes: 0,
            },
            other_resident_count: 2,
            requested_context_tokens: None,
        };
        let plan = LocalOffloadPlanner::plan(&input).expect("metal plan with projector memory");

        assert_eq!(plan.accelerator, AcceleratorKind::Metal);
        // Available memory drops by exactly the projector's reserved 2 GiB
        // relative to the zero-projector-memory metal test above (gib(4)).
        assert_eq!(plan.available_ram_bytes, gib(2));
        assert_eq!(plan.available_vram_bytes, gib(2));
        assert_eq!(plan.projector_placement, ProjectorPlacement::Gpu);
        assert!(plan.rationale.iter().any(|entry| entry.field == "projector_memory_bytes"
            && entry.explanation.contains("2.0 GB")));
    }

    #[test]
    fn offload_plan_falls_back_to_cpu_when_required_accelerator_missing() {
        let hardware = cpu_only_hardware(16, 12, 8);
        let input = OffloadPlanInput {
            hardware,
            model: OffloadModelProfile {
                weights_bytes: gib(2),
                estimated_ram_bytes: gib(2) + gib(1) / 4,
                estimated_vram_bytes: gib(2) + gib(1) / 4,
                required_accelerator: Some(AcceleratorKind::Cuda),
                has_vision_projector: false,
                projector_memory_bytes: 0,
            },
            reserved: zero_reserved(),
            other_resident_count: 0,
            requested_context_tokens: Some(4_096),
        };
        let plan = LocalOffloadPlanner::plan(&input).expect("cpu fallback plan");

        assert_eq!(plan.accelerator, AcceleratorKind::Cpu);
        assert_eq!(plan.gpu_layers, 0);
        assert!(plan
            .rationale
            .iter()
            .any(|entry| entry.field == "accelerator" && entry.explanation.contains("Cuda")));
        assert!(plan
            .improvement_suggestions
            .iter()
            .any(|message| message.contains("requires Cuda")));
        assert!(!plan
            .improvement_suggestions
            .iter()
            .any(|message| message.contains("No GPU or Metal acceleration was detected")));
    }

    #[test]
    fn offload_plan_input_validation_rejects_impossible_values() {
        let hardware = cpu_only_hardware(16, 10, 8);
        let base_model = OffloadModelProfile {
            weights_bytes: gib(4),
            estimated_ram_bytes: gib(5),
            estimated_vram_bytes: 0,
            required_accelerator: None,
            has_vision_projector: false,
            projector_memory_bytes: 0,
        };

        let zero_weights = OffloadPlanInput {
            hardware: hardware.clone(),
            model: OffloadModelProfile {
                weights_bytes: 0,
                ..base_model.clone()
            },
            reserved: zero_reserved(),
            other_resident_count: 0,
            requested_context_tokens: None,
        };
        assert!(matches!(
            LocalOffloadPlanner::plan(&zero_weights),
            Err(RuntimeAdapterError::InvalidOperationLimits { .. })
        ));

        let over_reserved = OffloadPlanInput {
            hardware: hardware.clone(),
            model: base_model.clone(),
            reserved: MemoryRequirement {
                ram_bytes: gib(17),
                vram_bytes: 0,
            },
            other_resident_count: 0,
            requested_context_tokens: None,
        };
        assert!(matches!(
            LocalOffloadPlanner::plan(&over_reserved),
            Err(RuntimeAdapterError::InvalidOperationLimits { .. })
        ));

        let zero_context = OffloadPlanInput {
            hardware,
            model: base_model,
            reserved: zero_reserved(),
            other_resident_count: 0,
            requested_context_tokens: Some(0),
        };
        assert!(matches!(
            LocalOffloadPlanner::plan(&zero_context),
            Err(RuntimeAdapterError::InvalidOperationLimits { .. })
        ));
    }
}
