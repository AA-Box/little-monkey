//! Production dependency assembly for the M3 runtime hub.
//!
//! This module owns concrete operating-system, HTTP, process, keychain, and
//! runtime implementations. The Tauri root only needs to construct and manage
//! one [`M3CommandState`]; no production dependency is supplied by the UI.

use crate::compatibility_hub::{
    request_offers_tool, CanonicalContent, CanonicalEmbeddingDatum, CanonicalEmbeddingRequest,
    CanonicalEmbeddingResponse, CanonicalInferenceRequest, CanonicalInferenceResponse,
    CanonicalMessage, CanonicalRole, CanonicalStreamEvent, CanonicalUsage, LanStateProtector,
    OsLanEntropy,
};
use crate::context_cache::{classify_context_failure, ContextFailureInput};
use crate::m3_commands::{M3CommandState, M3OwnedProcessShutdown};
use crate::m3_runtime_hub::{
    DefaultM3LanAccessFactory, HttpM3CatalogSource, M3AcceleratorCompatibility, M3AcceleratorStatus,
    M3CanonicalStreamSink, M3CatalogSource, M3Clock, M3ComponentCatalogEntry, M3ComponentHub,
    M3ComponentHubDependencies, M3ComponentSource, M3HardwareCompatibilityReport, M3HardwareProbe,
    M3HubConfig, M3HubError, M3HubFuture, M3HubResult, M3InferenceEngine, M3InstalledModelView,
    M3JetsonInfo, M3ModelCapabilities, M3OperationContext, M3RuntimeDriver, M3RuntimeHub,
    M3RuntimeHubDependencies, M3RuntimeKind, M3RuntimeReconciler, M3RuntimeStatusView,
    ReqwestM3DownloadTransport, RuntimeAdapterM3Driver, StaticM3ComponentSource, SystemM3Clock,
};
// MLX is Metal-only, so the module and everything that assembles it are compiled
// into the macOS build alone.
#[cfg(target_os = "macos")]
use crate::m3_runtime_hub::MlxM3Driver;
#[cfg(target_os = "macos")]
use crate::mlx_runtime::{
    self, CurrentHostMlxProbe, MlxError, MlxFuture, MlxGenerationRequest, MlxGenerationSummary,
    MlxHostCapabilities, MlxInstallLimits, MlxLaunchSpec, MlxModelCapabilities, MlxModelRecord,
    MlxOperationContext, MlxPackageInstaller, MlxProcessHandle, MlxProcessMetrics,
    MlxRuntimeAdapter, MlxRuntimeConfig, MlxServiceController, MlxSignatureVerifier,
    MlxStreamEvent, MlxStreamSink,
};
use crate::runtime_adapter::{
    AcceleratorKind, EndpointOrigin, EndpointPolicy, HardwareSnapshot, HttpTransport,
    ManagedLlamaCppAdapter, ManagedLogChunk, ManagedProcessController, ManagedProcessHandle,
    ManagedProcessSpec, ManagedProcessState, ManagedProcessStatus, ModelCapabilities,
    OllamaHttpAdapter, PlatformCapabilities, PortOwnership, ReqwestHttpTransport,
    ResidencyOwnership, RuntimeAdapter, RuntimeAdapterError, RuntimeFuture, RuntimeKind,
    RuntimeModel, RuntimeOperationContext, RuntimeOperationLimits,
};
use base64::Engine as _;
use futures_util::StreamExt;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
// Only the MLX release-key verifier checks a signature here.
#[cfg(target_os = "macos")]
use ring::signature;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
// Only the MLX service controller counts generated tokens atomically.
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
// `ps`-based resident-memory sampling, reached only from MLX metrics.
#[cfg(target_os = "macos")]
use std::process::Command;

const M3_DIRECTORY: &str = "m3";
const M3_COMPONENTS_DIRECTORY: &str = "m3-components";
const CATALOG_CONFIG_FILE: &str = "catalog-sources.json";
const CATALOG_CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_CONFIG_BYTES: u64 = 256 * 1024;
const COMPONENT_REGISTRY_FILE: &str = "component-registry.json";
const COMPONENT_REGISTRY_SCHEMA_VERSION: u32 = 1;
const COMPONENT_REGISTRY_SOURCE_ID: &str = "local";
const MAX_COMPONENT_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const OLLAMA_RUNTIME_ID: &str = "ollama";
const LLAMA_RUNTIME_ID: &str = "managed-llama";
const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
const LLAMA_ENDPOINT: &str = "http://127.0.0.1:8090";
const LLAMA_PORT: u16 = 8_090;
const MAX_INFERENCE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_INFERENCE_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.m3-lan";
const KEYCHAIN_ACCOUNT: &str = "lan-state-hmac-v1";
#[cfg(target_os = "macos")]
const MLX_RELEASE_KEY_ID: &str = "release-2026-1";
#[cfg(target_os = "macos")]
const MLX_RELEASE_PUBLIC_KEY_HEX: &str =
    "84db8c4dfdca72589631be1513f45083e893c9c373ba5be6e49928e43c7b828c";

fn lock<T>(mutex: &Mutex<T>) -> M3HubResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| M3HubError::LockPoisoned)
}

fn runtime_error(error: RuntimeAdapterError) -> M3HubError {
    M3HubError::Runtime(error.to_string())
}

fn now_ms() -> M3HubResult<u64> {
    SystemM3Clock.now_ms()
}

fn now_seconds() -> M3HubResult<u64> {
    Ok(now_ms()? / 1_000)
}

/// Cross-platform, point-in-time hardware probe used for fit decisions.
///
/// `system-memory` supplies physical byte counts on all supported desktop
/// targets. Metal is advertised only for an Apple-Silicon build, where CPU
/// and GPU share the same physical memory; unsupported accelerators are never
/// guessed from model names or environment variables.
#[derive(Default)]
pub struct SystemM3HardwareProbe;

impl crate::m3_runtime_hub::M3HardwareProbe for SystemM3HardwareProbe {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        let (total_ram_bytes, available_ram_bytes) =
            std::panic::catch_unwind(|| (system_memory::total(), system_memory::available()))
                .map_err(|_| {
                    M3HubError::Runtime("operating-system memory probe failed".to_string())
                })?;
        if total_ram_bytes == 0 || available_ram_bytes > total_ram_bytes {
            return Err(M3HubError::Runtime(
                "operating-system memory probe returned impossible values".to_string(),
            ));
        }
        let logical_cpu_count = std::thread::available_parallelism()
            .map_err(|error| M3HubError::Runtime(format!("CPU probe failed: {error}")))?
            .get()
            .try_into()
            .map_err(|_| M3HubError::Runtime("logical CPU count overflow".to_string()))?;
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let mut accelerators = Vec::new();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let mut accelerators = vec![crate::runtime_adapter::AcceleratorCapability {
            kind: AcceleratorKind::Metal,
            available: true,
            device_names: vec!["Apple Silicon unified GPU".to_string()],
            total_memory_bytes: Some(total_ram_bytes),
            available_memory_bytes: Some(available_ram_bytes),
        }];
        if let Some(cuda) = detect_nvidia_accelerator() {
            accelerators.push(cuda);
        }
        let snapshot = HardwareSnapshot {
            captured_at_ms: now_ms()?,
            total_ram_bytes,
            available_ram_bytes,
            logical_cpu_count,
            platform: PlatformCapabilities::current(accelerators),
        };
        snapshot.profile().map_err(runtime_error)?;
        Ok(snapshot)
    }

    /// Hardware Compatibility Matrix / "Driver Doctor" report. Every backend
    /// below is probed defensively: an absent tool, an absent device, or an
    /// OS/arch combination that backend cannot run on is expected, everyday
    /// output, not an error. This must never panic or fail merely because a
    /// GPU tool or driver is missing (that is the `ToolMissing`/`NotDetected`
    /// case), so a plain machine with no CUDA/ROCm/Vulkan installed still
    /// gets a complete, honest report.
    fn compatibility_report(&self) -> M3HubResult<M3HardwareCompatibilityReport> {
        let snapshot = crate::m3_runtime_hub::M3HardwareProbe::snapshot(self)?;
        Ok(build_compatibility_report(&snapshot))
    }
}

/// Conservative minimum NVIDIA driver major version for the CUDA 11+
/// toolkits modern llama.cpp/MLX-adjacent CUDA builds target (CUDA 11.0
/// requires driver >= 450.80.02 on Linux / >= 452.39 on Windows). This is a
/// heuristic floor used only to raise the `DriverTooOld` signal; it is not an
/// exhaustive per-CUDA-version compatibility table.
const MIN_CUDA_DRIVER_MAJOR: u32 = 450;

/// Conservative minimum GPU compute capability (Maxwell/5.0 or newer) this
/// app expects for CUDA acceleration. Older GPUs still enumerate fine via
/// `nvidia-smi` but are not expected to run current CUDA kernels.
const MIN_CUDA_COMPUTE_CAPABILITY: f64 = 5.0;

/// Outcome of attempting to run an external hardware-detection tool
/// (`nvidia-smi`, `rocm-smi`, `vulkaninfo`, ...). `Missing` covers both "the
/// binary is not on PATH" and any other failure to even spawn the process;
/// either way the backend's tooling could not be reached. `Output` covers
/// every case where the process actually ran, including a non-zero exit
/// (treated as "no parseable device").
enum ToolRun {
    Missing,
    Output(String),
}

fn run_hardware_tool(program: &str, args: &[&str]) -> ToolRun {
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {
            ToolRun::Output(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(_non_success) => ToolRun::Output(String::new()),
        Err(_spawn_failure) => ToolRun::Missing,
    }
}

fn unsupported_accelerator(
    kind: AcceleratorKind,
    summary: impl Into<String>,
) -> M3AcceleratorCompatibility {
    M3AcceleratorCompatibility {
        kind,
        status: M3AcceleratorStatus::Unsupported,
        summary: summary.into(),
        device_names: Vec::new(),
        driver_version: None,
        compute_capability: None,
        confirmed: true,
    }
}

fn tool_missing_accelerator(
    kind: AcceleratorKind,
    summary: impl Into<String>,
) -> M3AcceleratorCompatibility {
    M3AcceleratorCompatibility {
        kind,
        status: M3AcceleratorStatus::ToolMissing,
        summary: summary.into(),
        device_names: Vec::new(),
        driver_version: None,
        compute_capability: None,
        confirmed: true,
    }
}

fn not_detected_accelerator(
    kind: AcceleratorKind,
    summary: impl Into<String>,
) -> M3AcceleratorCompatibility {
    M3AcceleratorCompatibility {
        kind,
        status: M3AcceleratorStatus::NotDetected,
        summary: summary.into(),
        device_names: Vec::new(),
        driver_version: None,
        compute_capability: None,
        confirmed: true,
    }
}

/// Builds the full Hardware Compatibility Matrix report for every known
/// accelerator backend plus Jetson and hybrid-graphics detection. `os`/`arch`
/// come from the already-normalized [`HardwareSnapshot`] so this function is
/// pure with respect to its input and trivially unit-testable by constructing
/// a snapshot with any `os` string.
fn build_compatibility_report(snapshot: &HardwareSnapshot) -> M3HardwareCompatibilityReport {
    let os = snapshot.platform.os.as_str();

    let accelerators = vec![
        metal_compatibility(os),
        cuda_compatibility(os),
        rocm_compatibility(os),
        vulkan_compatibility(os),
        directml_compatibility(os),
    ];

    let jetson = jetson_info(os);

    let mut hybrid_graphics_detected = accelerators
        .iter()
        .filter(|entry| entry.status == M3AcceleratorStatus::Available)
        .count()
        > 1;
    let mut notes = Vec::new();

    for entry in &accelerators {
        if entry.device_names.len() > 1 {
            hybrid_graphics_detected = true;
            notes.push(format!(
                "{:?} reported {} devices ({}); hybrid/multi-GPU systems may need explicit device selection rather than an automatic pick.",
                entry.kind,
                entry.device_names.len(),
                entry.device_names.join(", ")
            ));
        }
    }

    let cuda_available = accelerators
        .iter()
        .any(|entry| entry.kind == AcceleratorKind::Cuda && entry.status == M3AcceleratorStatus::Available);
    let rocm_available = accelerators
        .iter()
        .any(|entry| entry.kind == AcceleratorKind::Rocm && entry.status == M3AcceleratorStatus::Available);
    if cuda_available && rocm_available {
        notes.push(
            "Both NVIDIA (CUDA) and AMD (ROCm) GPUs were detected. Little Monkey selects one runtime per model; mixed-vendor acceleration within a single model load is not supported."
                .to_string(),
        );
    }

    if jetson.detected {
        notes.push(
            "Jetson (Tegra) device detected; use Jetson-appropriate CUDA/TensorRT builds rather than desktop CUDA packages.".to_string(),
        );
    }

    M3HardwareCompatibilityReport {
        captured_at_ms: snapshot.captured_at_ms,
        os: snapshot.platform.os.clone(),
        arch: snapshot.platform.arch.clone(),
        accelerators,
        jetson,
        hybrid_graphics_detected,
        notes,
    }
}

struct MetalGpuDevice {
    name: String,
    metal_family: Option<String>,
}

/// Parses `system_profiler SPDisplaysDataType -json` output. This is a real
/// query (not a name/arch guess): it reports every GPU macOS itself
/// enumerates, including the iGPU+dGPU case on older Intel Macs. Returns
/// `Some(vec![])` when the tool ran but reported no GPU entries, and `None`
/// when the output could not be parsed as the expected JSON shape at all.
fn parse_system_profiler_displays(output: &str) -> Option<Vec<MetalGpuDevice>> {
    let value: Value = serde_json::from_str(output).ok()?;
    let entries = value.get("SPDisplaysDataType")?.as_array()?;
    let mut gpus = Vec::new();
    for entry in entries {
        let name = entry
            .get("sppci_model")
            .and_then(Value::as_str)
            .or_else(|| entry.get("_name").and_then(Value::as_str))
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let metal_family = entry
            .get("spdisplays_mtlgpufamilysupport")
            .and_then(Value::as_str)
            .map(str::to_string);
        gpus.push(MetalGpuDevice { name, metal_family });
    }
    Some(gpus)
}

fn metal_compatibility(os: &str) -> M3AcceleratorCompatibility {
    if os != "macos" {
        return unsupported_accelerator(
            AcceleratorKind::Metal,
            "Metal is an Apple-only graphics API and this OS is not macOS.",
        );
    }
    if let ToolRun::Output(stdout) =
        run_hardware_tool("system_profiler", &["SPDisplaysDataType", "-json"])
    {
        if let Some(gpus) = parse_system_profiler_displays(&stdout) {
            if gpus.is_empty() {
                return not_detected_accelerator(
                    AcceleratorKind::Metal,
                    "system_profiler ran but reported no GPU.",
                );
            }
            let device_names = gpus.iter().map(|gpu| gpu.name.clone()).collect::<Vec<_>>();
            let metal_family = gpus.iter().find_map(|gpu| gpu.metal_family.clone());
            return M3AcceleratorCompatibility {
                kind: AcceleratorKind::Metal,
                status: M3AcceleratorStatus::Available,
                summary: match &metal_family {
                    Some(family) => format!("Metal is available ({family})."),
                    None => "Metal is available.".to_string(),
                },
                device_names,
                driver_version: None,
                compute_capability: metal_family,
                confirmed: true,
            };
        }
    }
    // `system_profiler` was unavailable or returned output this app could not
    // parse. Fall back to an OS/arch-based assumption rather than failing the
    // whole report; `confirmed: false` makes clear this is an assumption, not
    // a direct query result, matching Apple Silicon's near-universal Metal
    // support.
    let assumed_available = std::env::consts::ARCH == "aarch64";
    if assumed_available {
        M3AcceleratorCompatibility {
            kind: AcceleratorKind::Metal,
            status: M3AcceleratorStatus::Available,
            summary: "system_profiler was unavailable; assuming Metal is available because this is Apple Silicon macOS.".to_string(),
            device_names: vec!["Apple Silicon unified GPU (assumed)".to_string()],
            driver_version: None,
            compute_capability: None,
            confirmed: false,
        }
    } else {
        M3AcceleratorCompatibility {
            kind: AcceleratorKind::Metal,
            status: M3AcceleratorStatus::NotDetected,
            summary: "system_profiler was unavailable and this is not Apple Silicon; Metal support could not be confirmed.".to_string(),
            device_names: Vec::new(),
            driver_version: None,
            compute_capability: None,
            confirmed: false,
        }
    }
}

struct NvidiaCompatInfo {
    device_names: Vec<String>,
    driver_version: Option<String>,
    min_compute_capability: Option<String>,
}

/// Parses `nvidia-smi --query-gpu=driver_version,compute_cap,name,memory.total,memory.free
/// --format=csv,noheader,nounits` output. Modeled after [`parse_nvidia_smi`]'s
/// structure (rightmost fields are the fixed-format numeric memory columns),
/// extended with the two extra fixed-position leading columns this richer
/// query adds.
fn parse_nvidia_smi_compat(output: &str) -> Option<NvidiaCompatInfo> {
    let mut device_names = Vec::new();
    let mut driver_version: Option<String> = None;
    let mut min_compute_capability: Option<f64> = None;
    let mut min_compute_capability_str: Option<String> = None;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 5 {
            return None;
        }
        let free_mib = fields[fields.len() - 1].parse::<u64>().ok()?;
        let total_mib = fields[fields.len() - 2].parse::<u64>().ok()?;
        let name = fields[2..fields.len() - 2].join(",");
        let name = name.trim();
        if name.is_empty() || total_mib == 0 || free_mib > total_mib {
            return None;
        }
        device_names.push(name.to_string());
        let driver = fields[0];
        if !driver.is_empty() && !driver.eq_ignore_ascii_case("n/a") && driver_version.is_none() {
            driver_version = Some(driver.to_string());
        }
        let compute_cap = fields[1];
        if let Ok(value) = compute_cap.parse::<f64>() {
            let should_replace = match min_compute_capability {
                Some(current) => value < current,
                None => true,
            };
            if should_replace {
                min_compute_capability = Some(value);
                min_compute_capability_str = Some(compute_cap.to_string());
            }
        }
    }
    if device_names.is_empty() {
        return None;
    }
    Some(NvidiaCompatInfo {
        device_names,
        driver_version,
        min_compute_capability: min_compute_capability_str,
    })
}

fn cuda_compatibility(os: &str) -> M3AcceleratorCompatibility {
    if !matches!(os, "linux" | "windows") {
        return unsupported_accelerator(
            AcceleratorKind::Cuda,
            "CUDA requires an NVIDIA GPU on Linux or Windows.",
        );
    }
    match run_hardware_tool(
        "nvidia-smi",
        &[
            "--query-gpu=driver_version,compute_cap,name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ],
    ) {
        ToolRun::Missing => tool_missing_accelerator(
            AcceleratorKind::Cuda,
            "nvidia-smi was not found on PATH. Install the NVIDIA driver to enable CUDA.",
        ),
        ToolRun::Output(stdout) => match parse_nvidia_smi_compat(&stdout) {
            None => not_detected_accelerator(
                AcceleratorKind::Cuda,
                "nvidia-smi ran but reported no NVIDIA GPU; falls back to CPU.",
            ),
            Some(info) => {
                let driver_major = info
                    .driver_version
                    .as_deref()
                    .and_then(|value| value.split('.').next())
                    .and_then(|value| value.parse::<u32>().ok());
                let compute_cap_value = info
                    .min_compute_capability
                    .as_deref()
                    .and_then(|value| value.parse::<f64>().ok());
                let driver_too_old = driver_major.is_some_and(|major| major < MIN_CUDA_DRIVER_MAJOR);
                let compute_too_old =
                    compute_cap_value.is_some_and(|value| value < MIN_CUDA_COMPUTE_CAPABILITY);
                if driver_too_old || compute_too_old {
                    M3AcceleratorCompatibility {
                        kind: AcceleratorKind::Cuda,
                        status: M3AcceleratorStatus::DriverTooOld,
                        summary: format!(
                            "NVIDIA GPU detected, but {} below what this app expects (driver >= {MIN_CUDA_DRIVER_MAJOR}.x, compute capability >= {MIN_CUDA_COMPUTE_CAPABILITY:.1}). CUDA acceleration may fail or fall back to CPU.",
                            match (driver_too_old, compute_too_old) {
                                (true, true) => "the driver version and GPU compute capability are",
                                (true, false) => "the driver version is",
                                _ => "the GPU compute capability is",
                            }
                        ),
                        device_names: info.device_names,
                        driver_version: info.driver_version,
                        compute_capability: info.min_compute_capability,
                        confirmed: true,
                    }
                } else {
                    M3AcceleratorCompatibility {
                        kind: AcceleratorKind::Cuda,
                        status: M3AcceleratorStatus::Available,
                        summary: "CUDA is available.".to_string(),
                        device_names: info.device_names,
                        driver_version: info.driver_version,
                        compute_capability: info.min_compute_capability,
                        confirmed: true,
                    }
                }
            }
        },
    }
}

struct RocmCompatInfo {
    device_names: Vec<String>,
    driver_version: Option<String>,
}

/// Parses `rocm-smi --showproductname --showdriverversion --csv` output,
/// modeled after [`parse_nvidia_smi`]'s tolerant, line-oriented structure.
/// The exact ROCm CSV column layout varies across ROCm releases, so this
/// parser only assumes: an optional header row starting with `device,`, then
/// one data row per GPU with the device id, product name, and (optionally) a
/// driver version as the first three comma-separated fields.
fn parse_rocm_smi(output: &str) -> Option<RocmCompatInfo> {
    let mut device_names = Vec::new();
    let mut driver_version: Option<String> = None;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.to_ascii_lowercase().starts_with("device,") {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 2 {
            continue;
        }
        let name = fields[1];
        if name.is_empty() {
            continue;
        }
        device_names.push(name.to_string());
        if let Some(version) = fields.get(2) {
            if !version.is_empty() && driver_version.is_none() {
                driver_version = Some((*version).to_string());
            }
        }
    }
    if device_names.is_empty() {
        None
    } else {
        Some(RocmCompatInfo {
            device_names,
            driver_version,
        })
    }
}

fn rocm_compatibility(os: &str) -> M3AcceleratorCompatibility {
    if !matches!(os, "linux" | "windows") {
        return unsupported_accelerator(
            AcceleratorKind::Rocm,
            "ROCm requires an AMD GPU on Linux (or a Windows ROCm/HIP build).",
        );
    }
    match run_hardware_tool(
        "rocm-smi",
        &["--showproductname", "--showdriverversion", "--csv"],
    ) {
        ToolRun::Missing => tool_missing_accelerator(
            AcceleratorKind::Rocm,
            "rocm-smi was not found on PATH. Install the ROCm stack to enable AMD GPU acceleration.",
        ),
        ToolRun::Output(stdout) => match parse_rocm_smi(&stdout) {
            None => not_detected_accelerator(
                AcceleratorKind::Rocm,
                "rocm-smi ran but reported no AMD GPU; falls back to CPU.",
            ),
            Some(info) => M3AcceleratorCompatibility {
                kind: AcceleratorKind::Rocm,
                status: M3AcceleratorStatus::Available,
                summary: match &info.driver_version {
                    Some(version) => format!("ROCm is available (driver {version})."),
                    None => "ROCm is available.".to_string(),
                },
                device_names: info.device_names,
                driver_version: info.driver_version,
                compute_capability: None,
                confirmed: true,
            },
        },
    }
}

struct VulkanDevice {
    name: String,
    driver_version: Option<String>,
    device_type: Option<String>,
}

/// Parses `vulkaninfo --summary` output. Devices appear as `GPU0:`, `GPU1:`,
/// ... header lines followed by indented `key = value` properties; this
/// parser only reads the three properties it needs (`deviceName`,
/// `driverVersion`, `deviceType`) and ignores everything else so it stays
/// resilient to `vulkaninfo` version differences.
fn parse_vulkaninfo_summary(output: &str) -> Option<Vec<VulkanDevice>> {
    let mut devices = Vec::new();
    let mut current: Option<VulkanDevice> = None;
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("GPU") {
            if let Some(digits) = rest.strip_suffix(':') {
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    if let Some(device) = current.take() {
                        devices.push(device);
                    }
                    current = Some(VulkanDevice {
                        name: String::new(),
                        driver_version: None,
                        device_type: None,
                    });
                    continue;
                }
            }
        }
        let Some(device) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "deviceName" => device.name = value.to_string(),
            "driverVersion" => {
                device.driver_version = value.split_whitespace().next().map(str::to_string);
            }
            "deviceType" => device.device_type = Some(value.to_string()),
            _ => {}
        }
    }
    if let Some(device) = current.take() {
        devices.push(device);
    }
    devices.retain(|device| !device.name.is_empty());
    if devices.is_empty() {
        None
    } else {
        Some(devices)
    }
}

fn vulkan_compatibility(os: &str) -> M3AcceleratorCompatibility {
    if !matches!(os, "linux" | "windows") {
        return unsupported_accelerator(
            AcceleratorKind::Vulkan,
            "Vulkan detection is limited to Linux and Windows in this build.",
        );
    }
    match run_hardware_tool("vulkaninfo", &["--summary"]) {
        ToolRun::Missing => tool_missing_accelerator(
            AcceleratorKind::Vulkan,
            "vulkaninfo was not found on PATH. Install the Vulkan runtime/SDK to enable Vulkan.",
        ),
        ToolRun::Output(stdout) => match parse_vulkaninfo_summary(&stdout) {
            None => not_detected_accelerator(
                AcceleratorKind::Vulkan,
                "vulkaninfo ran but reported no Vulkan-capable device; falls back to CPU.",
            ),
            Some(devices) => {
                let device_names = devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
                let driver_version = devices.first().and_then(|d| d.driver_version.clone());
                let has_discrete = devices
                    .iter()
                    .any(|d| d.device_type.as_deref().is_some_and(|t| t.contains("DISCRETE")));
                let has_integrated = devices
                    .iter()
                    .any(|d| d.device_type.as_deref().is_some_and(|t| t.contains("INTEGRATED")));
                let mut summary = format!(
                    "Vulkan is available ({} device{}).",
                    devices.len(),
                    if devices.len() == 1 { "" } else { "s" }
                );
                if has_discrete && has_integrated {
                    summary
                        .push_str(" Both an integrated and a discrete GPU were reported (hybrid graphics).");
                }
                M3AcceleratorCompatibility {
                    kind: AcceleratorKind::Vulkan,
                    status: M3AcceleratorStatus::Available,
                    summary,
                    device_names,
                    driver_version,
                    compute_capability: None,
                    confirmed: true,
                }
            }
        },
    }
}

/// Parses one GPU name per line, e.g. the output of
/// `Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name`.
fn parse_windows_video_controllers(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn directml_compatibility(os: &str) -> M3AcceleratorCompatibility {
    if os != "windows" {
        return unsupported_accelerator(
            AcceleratorKind::DirectMl,
            "DirectML is a Windows-only backend.",
        );
    }
    match run_hardware_tool(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
        ],
    ) {
        ToolRun::Missing => tool_missing_accelerator(
            AcceleratorKind::DirectMl,
            "The Windows GPU device query (PowerShell/WMI) was unavailable; DirectML support could not be checked.",
        ),
        ToolRun::Output(stdout) => {
            let device_names = parse_windows_video_controllers(&stdout);
            if device_names.is_empty() {
                not_detected_accelerator(
                    AcceleratorKind::DirectMl,
                    "No Windows video controller was reported; falls back to CPU.",
                )
            } else {
                M3AcceleratorCompatibility {
                    kind: AcceleratorKind::DirectMl,
                    status: M3AcceleratorStatus::Available,
                    // Deliberately not overclaiming: only a display adapter's
                    // presence was confirmed, not that the DirectML runtime
                    // path itself works end-to-end.
                    summary: "A GPU was detected via Windows device enumeration, but DirectML runtime support is unconfirmed (not verified end-to-end); treat as best-effort.".to_string(),
                    device_names,
                    driver_version: None,
                    compute_capability: None,
                    confirmed: false,
                }
            }
        }
    }
}

fn jetson_model_from_tegra_release(content: &str) -> Option<String> {
    let line = content.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn jetson_model_from_device_tree(raw: &[u8]) -> Option<String> {
    let model = String::from_utf8_lossy(raw);
    let model = model.trim_matches(char::from(0)).trim();
    if !model.is_empty() && model.to_ascii_lowercase().contains("jetson") {
        Some(model.to_string())
    } else {
        None
    }
}

/// Detects NVIDIA Jetson (Tegra) boards on Linux by checking
/// `/etc/nv_tegra_release` (present only on Jetson/L4T images) and, failing
/// that, `/proc/device-tree/model` for a "Jetson" model string. Both files
/// are simply absent on non-Jetson machines, which is the normal, expected
/// `detected: false` case.
fn jetson_info(os: &str) -> M3JetsonInfo {
    if os != "linux" {
        return M3JetsonInfo {
            detected: false,
            model: None,
        };
    }
    if let Ok(release) = std::fs::read_to_string("/etc/nv_tegra_release") {
        return M3JetsonInfo {
            detected: true,
            model: jetson_model_from_tegra_release(&release),
        };
    }
    if let Ok(bytes) = std::fs::read("/proc/device-tree/model") {
        if let Some(model) = jetson_model_from_device_tree(&bytes) {
            return M3JetsonInfo {
                detected: true,
                model: Some(model),
            };
        }
    }
    M3JetsonInfo {
        detected: false,
        model: None,
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn detect_nvidia_accelerator() -> Option<crate::runtime_adapter::AcceleratorCapability> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_nvidia_accelerator() -> Option<crate::runtime_adapter::AcceleratorCapability> {
    None
}

#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn parse_nvidia_smi(output: &str) -> Option<crate::runtime_adapter::AcceleratorCapability> {
    const MIB: u64 = 1024 * 1024;
    let mut device_names = Vec::new();
    let mut total_memory_bytes = 0_u64;
    let mut available_memory_bytes = 0_u64;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.rsplitn(3, ',').map(str::trim);
        let free_mib = fields.next()?.parse::<u64>().ok()?;
        let total_mib = fields.next()?.parse::<u64>().ok()?;
        let name = fields.next()?.trim();
        if name.is_empty() || total_mib == 0 || free_mib > total_mib {
            return None;
        }
        device_names.push(name.to_string());
        total_memory_bytes = total_memory_bytes.saturating_add(total_mib.saturating_mul(MIB));
        available_memory_bytes =
            available_memory_bytes.saturating_add(free_mib.saturating_mul(MIB));
    }
    if device_names.is_empty() {
        return None;
    }
    Some(crate::runtime_adapter::AcceleratorCapability {
        kind: AcceleratorKind::Cuda,
        available: true,
        device_names,
        total_memory_bytes: Some(total_memory_bytes),
        available_memory_bytes: Some(available_memory_bytes),
    })
}

/// HMAC-SHA256 protection whose key is generated from OS CSPRNG entropy and
/// stored only in the operating-system keychain.
pub struct KeychainLanStateProtector {
    key: hmac::Key,
    protector_id: String,
}

impl KeychainLanStateProtector {
    pub fn load_or_create() -> M3HubResult<Self> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .map_err(|error| M3HubError::State(format!("access M3 LAN keychain: {error}")))?;
        let key_bytes = match entry.get_password() {
            Ok(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| M3HubError::State("M3 LAN keychain value is corrupt".to_string()))?,
            Err(keyring::Error::NoEntry) => {
                let mut generated = vec![0_u8; 32];
                SystemRandom::new().fill(&mut generated).map_err(|_| {
                    M3HubError::State("operating-system random source failed".to_string())
                })?;
                entry
                    .set_password(&base64::engine::general_purpose::STANDARD.encode(&generated))
                    .map_err(|error| {
                        M3HubError::State(format!("store M3 LAN key in keychain: {error}"))
                    })?;
                generated
            }
            Err(error) => {
                return Err(M3HubError::State(format!(
                    "read M3 LAN key from keychain: {error}"
                )))
            }
        };
        Self::from_key(key_bytes)
    }

    fn from_key(key_bytes: Vec<u8>) -> M3HubResult<Self> {
        if key_bytes.len() != 32 {
            return Err(M3HubError::State(
                "M3 LAN keychain value has the wrong length".to_string(),
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&key_bytes));
        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key_bytes),
            protector_id: format!("keychain-hmac-sha256-{}", &digest[..24]),
        })
    }
}

impl LanStateProtector for KeychainLanStateProtector {
    fn protector_id(&self) -> &str {
        &self.protector_id
    }

    fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
        Ok(hmac::sign(&self.key, canonical_state).as_ref().to_vec())
    }

    fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
        hmac::verify(&self.key, canonical_state, tag)
            .map_err(|_| "M3 LAN state authentication tag mismatch".to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProductionCatalogConfig {
    schema_version: u32,
    sources: Vec<M3CatalogSourceConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CatalogSourceConfig {
    pub source_id: String,
    pub endpoint: String,
}

pub fn catalog_source_configs(root: &Path) -> M3HubResult<Vec<M3CatalogSourceConfig>> {
    let path = root.join(CATALOG_CONFIG_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 catalog configuration",
                path,
                source: error,
            })
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CATALOG_CONFIG_BYTES {
        return Err(M3HubError::State(
            "M3 catalog configuration must be a bounded regular file".to_string(),
        ));
    }
    let bytes = fs::read(&path).map_err(|source| M3HubError::Io {
        operation: "read M3 catalog configuration",
        path: path.clone(),
        source,
    })?;
    let config: ProductionCatalogConfig = serde_json::from_slice(&bytes)?;
    if config.schema_version != CATALOG_CONFIG_SCHEMA_VERSION || config.sources.len() > 32 {
        return Err(M3HubError::State(
            "M3 catalog configuration version/count is unsupported".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in &config.sources {
        if !ids.insert(entry.source_id.clone()) {
            return Err(M3HubError::State(
                "M3 catalog source ids must be unique".to_string(),
            ));
        }
        // Constructing the production source is the canonical validation for
        // identifiers, HTTPS/loopback policy, and bounded HTTP behavior.
        HttpM3CatalogSource::new(entry.source_id.clone(), &entry.endpoint)?;
    }
    Ok(config.sources)
}

fn catalog_sources_from_configs(
    configs: &[M3CatalogSourceConfig],
) -> M3HubResult<Vec<Arc<dyn M3CatalogSource>>> {
    if configs.len() > 32 {
        return Err(M3HubError::State(
            "at most 32 M3 catalog sources can be configured".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    configs
        .iter()
        .map(|entry| {
            if !ids.insert(entry.source_id.clone()) {
                return Err(M3HubError::State(
                    "M3 catalog source ids must be unique".to_string(),
                ));
            }
            HttpM3CatalogSource::new(entry.source_id.clone(), &entry.endpoint)
                .map(|source| Arc::new(source) as Arc<dyn M3CatalogSource>)
        })
        .collect()
}

fn load_catalog_sources(root: &Path) -> M3HubResult<Vec<Arc<dyn M3CatalogSource>>> {
    let configs = catalog_source_configs(root)?;
    catalog_sources_from_configs(&configs)
}

pub fn replace_catalog_source_configs(
    hub: &M3RuntimeHub,
    configs: Vec<M3CatalogSourceConfig>,
) -> M3HubResult<Vec<M3CatalogSourceConfig>> {
    let sources = catalog_sources_from_configs(&configs)?;
    let document = ProductionCatalogConfig {
        schema_version: CATALOG_CONFIG_SCHEMA_VERSION,
        sources: configs.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&document)?;
    if bytes.len() as u64 > MAX_CATALOG_CONFIG_BYTES {
        return Err(M3HubError::State(
            "M3 catalog configuration exceeds its byte limit".to_string(),
        ));
    }
    let root = hub.root();
    ensure_private_directory(root)?;
    let path = root.join(CATALOG_CONFIG_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(M3HubError::State(
                "M3 catalog configuration target is not a regular file".to_string(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 catalog configuration target",
                path,
                source,
            })
        }
    }
    let temporary = root.join(format!(".catalog-sources-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(|source| M3HubError::Io {
        operation: "create staged M3 catalog configuration",
        path: temporary.clone(),
        source,
    })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "write staged M3 catalog configuration",
            path: temporary,
            source,
        });
    }
    if let Err(source) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "publish M3 catalog configuration",
            path,
            source,
        });
    }
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| M3HubError::Io {
            operation: "sync M3 catalog configuration directory",
            path: root.to_path_buf(),
            source,
        })?;
    hub.replace_catalog_sources(sources)?;
    Ok(configs)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProductionComponentRegistry {
    schema_version: u32,
    entries: Vec<M3ComponentCatalogEntry>,
}

/// Loads the app's local, operator-editable registry of known runtime
/// component versions (llama.cpp/MLX/tokenizer/converter/projector/
/// accelerator-support builds).
///
/// There is no real upstream binary registry/CDN this app can verify and
/// hit today for these artifacts, so — mirroring the pluggable
/// `M3CatalogSource` pattern used for model catalogs — this reads a local
/// JSON file an operator populates with entries they have independently
/// vetted (a real source URL plus the sha256 they verified against it),
/// rather than hardcoding a call to a registry this environment cannot
/// confirm works. A missing file means an empty registry (no components
/// advertised as installable yet), which is the honest default until an
/// operator supplies real, verified entries.
pub fn component_registry_entries(root: &Path) -> M3HubResult<Vec<M3ComponentCatalogEntry>> {
    let path = root.join(COMPONENT_REGISTRY_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 component registry",
                path,
                source: error,
            })
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_COMPONENT_REGISTRY_BYTES {
        return Err(M3HubError::State(
            "M3 component registry must be a bounded regular file".to_string(),
        ));
    }
    let bytes = fs::read(&path).map_err(|source| M3HubError::Io {
        operation: "read M3 component registry",
        path: path.clone(),
        source,
    })?;
    let registry: ProductionComponentRegistry = serde_json::from_slice(&bytes)?;
    if registry.schema_version != COMPONENT_REGISTRY_SCHEMA_VERSION {
        return Err(M3HubError::State(
            "M3 component registry version is unsupported".to_string(),
        ));
    }
    // Constructing the source is the canonical validation for every entry.
    let entries = adopt_into_registry(registry.entries);
    StaticM3ComponentSource::new(COMPONENT_REGISTRY_SOURCE_ID, entries.clone())?;
    Ok(entries)
}

/// Restamps every entry as belonging to the local registry.
///
/// A catalog file is written by whoever published the component and carries
/// *their* `sourceId` — the MLX release workflow, for one, publishes
/// `little-monkey-mlx`. The local registry is a single source whose id is
/// [`COMPONENT_REGISTRY_SOURCE_ID`], and `StaticM3ComponentSource::new` refuses
/// any entry claiming a different one. Without this, importing a published
/// catalog fails with "entry source differs from the configured source" and the
/// component is uninstallable — which is what happened to every catalog this
/// project has ever published.
///
/// Rewriting rather than rejecting is right because the field is the
/// publisher's claim about where the entry came from, and once it is in this
/// file the answer is "the local registry" no matter who wrote it. Nothing else
/// reads `source_id`: identity is `component_id`, and a version is keyed on
/// version/digest/URL. The digest and the publisher-key check that actually
/// establish trust are untouched.
fn adopt_into_registry(
    entries: Vec<M3ComponentCatalogEntry>,
) -> Vec<M3ComponentCatalogEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            entry.source_id = COMPONENT_REGISTRY_SOURCE_ID.to_string();
            entry
        })
        .collect()
}

fn component_sources_from_entries(
    entries: &[M3ComponentCatalogEntry],
) -> M3HubResult<Vec<Arc<dyn M3ComponentSource>>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![Arc::new(StaticM3ComponentSource::new(
        COMPONENT_REGISTRY_SOURCE_ID,
        entries.to_vec(),
    )?)])
}

fn load_component_sources(root: &Path) -> M3HubResult<Vec<Arc<dyn M3ComponentSource>>> {
    component_sources_from_entries(&component_registry_entries(root)?)
}

/// Replaces the local component registry file and the hub's in-memory
/// sources together, mirroring `replace_catalog_source_configs`'s
/// validate-then-atomically-publish shape.
pub fn replace_component_registry_entries(
    hub: &M3ComponentHub,
    entries: Vec<M3ComponentCatalogEntry>,
) -> M3HubResult<Vec<M3ComponentCatalogEntry>> {
    // Restamped before validation, so a catalog published by someone else
    // imports instead of being refused over a field this registry owns. The
    // persisted file, the in-memory source and the value returned to the caller
    // are all the restamped set, so none of the three can disagree.
    let entries = adopt_into_registry(entries);
    let sources = component_sources_from_entries(&entries)?;
    let document = ProductionComponentRegistry {
        schema_version: COMPONENT_REGISTRY_SCHEMA_VERSION,
        entries: entries.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&document)?;
    if bytes.len() as u64 > MAX_COMPONENT_REGISTRY_BYTES {
        return Err(M3HubError::State(
            "M3 component registry exceeds its byte limit".to_string(),
        ));
    }
    let root = hub.root();
    ensure_private_directory(root)?;
    let path = root.join(COMPONENT_REGISTRY_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(M3HubError::State(
                "M3 component registry target is not a regular file".to_string(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 component registry target",
                path,
                source,
            })
        }
    }
    let temporary = root.join(format!(".component-registry-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(|source| M3HubError::Io {
        operation: "create staged M3 component registry",
        path: temporary.clone(),
        source,
    })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "write staged M3 component registry",
            path: temporary,
            source,
        });
    }
    if let Err(source) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "publish M3 component registry",
            path,
            source,
        });
    }
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| M3HubError::Io {
            operation: "sync M3 component registry directory",
            path: root.to_path_buf(),
            source,
        })?;
    hub.replace_sources(sources)?;
    Ok(entries)
}

/// Canonical inference implementation for Ollama and llama.cpp's local
/// OpenAI-compatible chat-completions endpoints.
pub struct OpenAiCompatibleM3InferenceEngine {
    endpoint: EndpointOrigin,
    client: reqwest::Client,
    active: Mutex<BTreeMap<String, CancellationToken>>,
}

impl OpenAiCompatibleM3InferenceEngine {
    pub fn new(endpoint: &str) -> M3HubResult<Self> {
        let endpoint =
            EndpointOrigin::parse(endpoint, EndpointPolicy::LoopbackOnly).map_err(runtime_error)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            endpoint,
            client,
            active: Mutex::new(BTreeMap::new()),
        })
    }

    fn begin_request(&self, request_id: &str) -> M3HubResult<CancellationToken> {
        if request_id.is_empty()
            || request_id.len() > 512
            || request_id.chars().any(char::is_control)
        {
            return Err(M3HubError::Runtime(
                "invalid inference request id".to_string(),
            ));
        }
        let token = CancellationToken::new();
        let mut active = lock(&self.active)?;
        if active.contains_key(request_id) {
            return Err(M3HubError::Conflict(format!(
                "inference request {request_id} is already active"
            )));
        }
        active.insert(request_id.to_string(), token.clone());
        Ok(token)
    }

    fn finish_request(&self, request_id: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(request_id);
        }
    }

    /// Refuses the request before it is sent when its prompt is over the process's
    /// context budget (roadmap K11).
    ///
    /// # Why before, and why an exact count
    ///
    /// The acceptance asks for a budget "enforced as a limit rather than
    /// discovered as a failure". Discovering it means the runtime evaluates the
    /// prompt, refuses or shifts its context, and `classify_context_failure`
    /// explains what already happened. Enforcing it means the request never
    /// leaves.
    ///
    /// That needs the exact prompt length, which only the runtime can give:
    /// `POST /apply-template` renders the exact string a completion would send
    /// (template included — the template alone is tens of tokens), and
    /// `POST /tokenize` returns its exact tokens. This app has no tokenizer, and
    /// enforcing a limit against an estimate would refuse real work for a made-up
    /// reason.
    ///
    /// # Fail-closed, and why that is the honest direction here
    ///
    /// A process with no budget — every process today — returns immediately,
    /// having sent nothing. Only when a budget *is* set do the two pre-flight
    /// calls happen, and if either cannot produce a count the request is refused
    /// with that reason rather than sent unchecked. A runtime without a tokenizer
    /// (Ollama, MLX) therefore reports that the budget cannot be enforced instead
    /// of quietly not enforcing it — "I set a limit and it silently did nothing"
    /// is the failure this direction exists to prevent.
    async fn enforce_context_budget(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let Some(budget) =
            crate::run_scope::current_process().and_then(|process| process.max_context_tokens())
        else {
            return Ok(());
        };
        let prompt_tokens = self
            .count_prompt_tokens(body, cancellation, context)
            .await?;
        match crate::context_cache::check_context_budget(prompt_tokens, Some(budget)) {
            crate::context_cache::ContextBudgetVerdict::Within => Ok(()),
            verdict => {
                // The class's policy decides what being over the budget *means*:
                // compact and carry on, or stop. Carrying it in the refusal is
                // what lets a caller act rather than only report.
                let policy = crate::run_scope::current_process()
                    .and_then(|process| process.class())
                    .map(crate::context_cache::context_policy);
                Err(M3HubError::ContextBudget {
                    // No class, no policy: the bare code says a budget was hit
                    // and stops there, rather than defaulting to one of the two
                    // answers and telling a client to act on a guess.
                    code: policy.map_or("context_budget", |policy| policy.code()),
                    message: verdict
                        .refusal_under(policy)
                        .unwrap_or_else(|| "context budget exceeded".to_string()),
                })
            }
        }
    }

    /// The exact prompt-token count for `body`, from the runtime itself.
    ///
    /// Two loopback calls, measured at well under a millisecond each against the
    /// pinned build — affordable per turn, and only paid when a budget is set.
    /// Both are `POST`s the OpenAI-compatible surface does not define, so a
    /// non-2xx here means "this runtime has no tokenizer", which is reported as
    /// an unenforceable budget rather than swallowed.
    async fn count_prompt_tokens(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<u64> {
        let messages = body.get("messages").cloned().unwrap_or(Value::Null);
        let rendered = self
            .post_json(
                "/apply-template",
                &json!({ "messages": messages }),
                cancellation,
                context,
            )
            .await?;
        // llama-server answers with the rendered string under `prompt`.
        let prompt = rendered.get("prompt").and_then(Value::as_str).ok_or_else(|| {
            M3HubError::Runtime(
                "this runtime did not render a prompt, so the context budget set for this process cannot be enforced against it".to_string(),
            )
        })?;
        let tokenized = self
            .post_json(
                "/tokenize",
                &json!({ "content": prompt }),
                cancellation,
                context,
            )
            .await?;
        let tokens = tokenized.get("tokens").and_then(Value::as_array).ok_or_else(|| {
            M3HubError::Runtime(
                "this runtime did not return a token count, so the context budget set for this process cannot be enforced against it".to_string(),
            )
        })?;
        Ok(tokens.len() as u64)
    }

    /// One bounded loopback `POST`, for the two pre-flight endpoints.
    async fn post_json(
        &self,
        path: &str,
        body: &Value,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<Value> {
        let url = self.endpoint.url(path).map_err(runtime_error)?;
        let encoded = serde_json::to_vec(body)?;
        let operation = async {
            tokio::select! {
                _ = context.cancellation.cancelled() => Err(M3HubError::Cancelled { operation: format!("local {path} request") }),
                _ = cancellation.cancelled() => Err(M3HubError::Cancelled { operation: format!("local {path} request") }),
                response = crate::egress::send(self.client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(encoded)) => {
                    response.map_err(|error| M3HubError::Transport(error.to_string()))
                }
            }
        };
        let response = tokio::time::timeout(Duration::from_millis(context.timeout_ms), operation)
            .await
            .map_err(|_| M3HubError::Timeout {
                operation: format!("local {path} request"),
                timeout_ms: context.timeout_ms,
            })??;
        if !response.status().is_success() {
            return Err(M3HubError::Runtime(format!(
                "this runtime has no {path} endpoint (HTTP {}), so the context budget set for this process cannot be enforced against it",
                response.status()
            )));
        }
        let bytes = read_bounded_response(
            response,
            MAX_INFERENCE_RESPONSE_BYTES,
            cancellation,
            context,
        )
        .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn send(
        &self,
        request: &CanonicalInferenceRequest,
        stream: bool,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<reqwest::Response> {
        let body = openai_request_body(request, stream)?;
        self.enforce_context_budget(&body, cancellation, context)
            .await?;
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_INFERENCE_REQUEST_BYTES {
            return Err(M3HubError::Runtime(
                "canonical inference request exceeds the production byte limit".to_string(),
            ));
        }
        let url = self
            .endpoint
            .url("/v1/chat/completions")
            .map_err(runtime_error)?;
        let operation = async {
            tokio::select! {
                _ = context.cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local inference request".to_string() }),
                _ = cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local inference request".to_string() }),
                response = crate::egress::send(self.client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(encoded)) => {
                    response.map_err(|error| M3HubError::Transport(error.to_string()))
                }
            }
        };
        let response = tokio::time::timeout(Duration::from_millis(context.timeout_ms), operation)
            .await
            .map_err(|_| M3HubError::Timeout {
                operation: "local inference request".to_string(),
                timeout_ms: context.timeout_ms,
            })??;
        if !response.status().is_success() {
            let status = response.status();
            let detail = read_bounded_response(response, 64 * 1024, cancellation, context).await?;
            let detail_text = String::from_utf8_lossy(&detail).trim().to_string();
            // A runtime's own error body is the most reliable place to spot a
            // context/cache/memory-related failure (e.g. llama-server's
            // "the request exceeds the available context size, try
            // increasing it"), so classify it here and fold the explanation
            // into the message the user actually sees — this is a best-effort
            // text/status classification only; it never fabricates numeric
            // context/memory figures this call site doesn't have.
            let classification = classify_context_failure(&ContextFailureInput {
                error_text: Some(detail_text.clone()),
                http_status: Some(status.as_u16()),
                ..Default::default()
            });
            return Err(M3HubError::Runtime(match classification {
                Some(classification) => format!(
                    "local inference returned HTTP {status} [context:{}] {} (raw: {detail_text})",
                    classification.class.slug(),
                    classification.explanation
                ),
                None => format!("local inference returned HTTP {status}: {detail_text}"),
            }));
        }
        Ok(response)
    }

    async fn complete_inner(
        &self,
        request: &CanonicalInferenceRequest,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<CanonicalInferenceResponse> {
        let response = self.send(request, false, cancellation, context).await?;
        let bytes = read_bounded_response(
            response,
            MAX_INFERENCE_RESPONSE_BYTES,
            cancellation,
            context,
        )
        .await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let response = parse_openai_response(&value, request)?;
        note_measured_reuse(&response.usage);
        Ok(response)
    }

    async fn stream_inner(
        &self,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let response = self.send(request, true, cancellation, context).await?;
        parse_openai_sse(response, request, sink, cancellation, context).await
    }

    /// Real `POST {endpoint}/v1/embeddings` call. This is the genuine gap
    /// closer: Ollama's local daemon serves an OpenAI-compatible embeddings
    /// endpoint alongside chat on the same base URL, so this engine
    /// (constructed with `OLLAMA_ENDPOINT`) produces real vectors. The same
    /// engine constructed with `LLAMA_ENDPOINT` (the managed llama.cpp chat
    /// instance, started without `--embeddings`) will reach a real HTTP
    /// endpoint that itself returns an honest error — never a fabricated
    /// vector.
    async fn embed_inner(
        &self,
        request: &CanonicalEmbeddingRequest,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<CanonicalEmbeddingResponse> {
        let body = json!({
            "model": request.model,
            "input": request.input,
        });
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_INFERENCE_REQUEST_BYTES {
            return Err(M3HubError::Runtime(
                "canonical embeddings request exceeds the production byte limit".to_string(),
            ));
        }
        let url = self.endpoint.url("/v1/embeddings").map_err(runtime_error)?;
        let operation = async {
            tokio::select! {
                _ = context.cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local embeddings request".to_string() }),
                _ = cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local embeddings request".to_string() }),
                response = crate::egress::send(self.client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(encoded)) => {
                    response.map_err(|error| M3HubError::Transport(error.to_string()))
                }
            }
        };
        let response = tokio::time::timeout(Duration::from_millis(context.timeout_ms), operation)
            .await
            .map_err(|_| M3HubError::Timeout {
                operation: "local embeddings request".to_string(),
                timeout_ms: context.timeout_ms,
            })??;
        if !response.status().is_success() {
            let status = response.status();
            let detail = read_bounded_response(response, 64 * 1024, cancellation, context).await?;
            return Err(M3HubError::Runtime(format!(
                "local embeddings endpoint returned HTTP {status}: {}",
                String::from_utf8_lossy(&detail).trim()
            )));
        }
        let bytes = read_bounded_response(
            response,
            MAX_INFERENCE_RESPONSE_BYTES,
            cancellation,
            context,
        )
        .await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        parse_openai_embeddings_response(&value, request)
    }
}

impl M3InferenceEngine for OpenAiCompatibleM3InferenceEngine {
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        Box::pin(async move {
            let cancellation = self.begin_request(&request.request_id)?;
            let result = self.complete_inner(request, &cancellation, context).await;
            self.finish_request(&request.request_id);
            result
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let cancellation = self.begin_request(&request.request_id)?;
            let result = self
                .stream_inner(request, sink, &cancellation, context)
                .await;
            self.finish_request(&request.request_id);
            result
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        Box::pin(async move {
            let active = lock(&self.active)?;
            if let Some(token) = active.get(request_id) {
                token.cancel();
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn embed<'a>(
        &'a self,
        request: &'a CanonicalEmbeddingRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalEmbeddingResponse> {
        Box::pin(async move {
            let cancellation = self.begin_request(&request.request_id)?;
            let result = self.embed_inner(request, &cancellation, context).await;
            self.finish_request(&request.request_id);
            result
        })
    }
}

/// Enforces the selected runtime's live model capability inventory before an
/// HTTP request reaches its inference endpoint. This is deliberately a
/// separate layer from wire translation: unsupported tools/structured output
/// fail locally even when an older backend would otherwise accept and ignore
/// those fields.
struct CapabilityCheckedInferenceEngine {
    adapter: Arc<dyn RuntimeAdapter>,
    inner: Arc<OpenAiCompatibleM3InferenceEngine>,
    structured_output_models: BTreeSet<String>,
}

impl CapabilityCheckedInferenceEngine {
    async fn validate(
        &self,
        request: &CanonicalInferenceRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        let runtime_context = RuntimeOperationContext::new(limits, context.cancellation.clone());
        let inventory = self
            .adapter
            .inventory(&runtime_context)
            .await
            .map_err(runtime_error)?;
        let model = inventory
            .models
            .iter()
            .find(|model| model.model_id == request.model)
            .ok_or_else(|| M3HubError::NotFound(format!("runtime model {}", request.model)))?;
        if !model.capabilities.chat {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise chat inference",
                request.model
            )));
        }
        let uses_tools = !request.tools.is_empty()
            || request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| {
                    matches!(
                        content,
                        CanonicalContent::ToolUse { .. } | CanonicalContent::ToolResult { .. }
                    )
                });
        if uses_tools && !model.capabilities.tool_calling {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise tool calling",
                request.model
            )));
        }
        if request.response_schema.is_some()
            && !self.structured_output_models.contains(&request.model)
        {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise structured output",
                request.model
            )));
        }
        Ok(())
    }

    async fn validate_embed(
        &self,
        request: &CanonicalEmbeddingRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        let runtime_context = RuntimeOperationContext::new(limits, context.cancellation.clone());
        let inventory = self
            .adapter
            .inventory(&runtime_context)
            .await
            .map_err(runtime_error)?;
        let model = inventory
            .models
            .iter()
            .find(|model| model.model_id == request.model)
            .ok_or_else(|| M3HubError::NotFound(format!("runtime model {}", request.model)))?;
        if !model.capabilities.embeddings {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise embeddings",
                request.model
            )));
        }
        Ok(())
    }
}

impl M3InferenceEngine for CapabilityCheckedInferenceEngine {
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        Box::pin(async move {
            self.validate(request, context).await?;
            self.inner.complete(request, context).await
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            self.validate(request, context).await?;
            self.inner.stream(request, sink, context).await
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        self.inner.cancel(request_id, context)
    }

    fn embed<'a>(
        &'a self,
        request: &'a CanonicalEmbeddingRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalEmbeddingResponse> {
        Box::pin(async move {
            self.validate_embed(request, context).await?;
            self.inner.embed(request, context).await
        })
    }
}

/// `pub(crate)`: reused directly (not mocked) by `chat_template_lab.rs`'s
/// compose-direction fixtures, so the compatibility lab exercises the exact
/// wire-body builder real inference traffic uses.
pub(crate) fn openai_request_body(
    request: &CanonicalInferenceRequest,
    stream: bool,
) -> M3HubResult<Value> {
    let messages = request
        .messages
        .iter()
        .flat_map(openai_messages)
        .collect::<M3HubResult<Vec<_>>>()?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": tool.strict
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(stream));
    body.insert(
        "max_tokens".to_string(),
        Value::Number(request.max_output_tokens.into()),
    );
    if let Some(temperature) = request.temperature {
        body.insert(
            "temperature".to_string(),
            serde_json::Number::from_f64(temperature)
                .map(Value::Number)
                .ok_or_else(|| M3HubError::Runtime("temperature is not finite".to_string()))?,
        );
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
    if let Some(schema) = &request.response_schema {
        body.insert(
            "response_format".to_string(),
            json!({"type":"json_schema","json_schema":{"name":"response","strict":true,"schema":schema}}),
        );
    }
    if stream {
        body.insert("stream_options".to_string(), json!({"include_usage":true}));
    }
    Ok(Value::Object(body))
}

fn openai_messages(message: &CanonicalMessage) -> Vec<M3HubResult<Value>> {
    let role = match message.role {
        CanonicalRole::System => "system",
        CanonicalRole::User => "user",
        CanonicalRole::Assistant => "assistant",
        CanonicalRole::Tool => "tool",
    };
    if message.role == CanonicalRole::Tool {
        return message
            .content
            .iter()
            .map(|content| match content {
                CanonicalContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Ok(json!({
                    "role":"tool",
                    "tool_call_id":tool_use_id,
                    "content": if *is_error { format!("Error: {content}") } else { content.clone() }
                })),
                _ => Err(M3HubError::Runtime(
                    "tool-role messages may contain only tool results".to_string(),
                )),
            })
            .collect();
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    // Image content blocks (ROADMAP Phase 8 item 12): collected as OpenAI
    // vision-style `data:` URIs. Only populated when the message actually
    // carries an image, so a plain text/tool message composes exactly as
    // before (a bare string or `null` `content`), matching every existing
    // wire fixture unchanged.
    let mut images = Vec::new();
    for content in &message.content {
        match content {
            CanonicalContent::Text { text: value } => text.push_str(value),
            CanonicalContent::ToolUse { id, name, input }
                if message.role == CanonicalRole::Assistant =>
            {
                calls.push(json!({
                    "id":id,
                    "type":"function",
                    "function":{"name":name,"arguments":input.to_string()}
                }));
            }
            CanonicalContent::Image {
                mime_type,
                data_base64,
            } => images.push(CanonicalContent::image_data_url(mime_type, data_base64)),
            CanonicalContent::ToolResult { .. } => {
                return vec![Err(M3HubError::Runtime(
                    "tool results require a tool-role message".to_string(),
                ))]
            }
            CanonicalContent::ToolUse { .. } => {
                return vec![Err(M3HubError::Runtime(
                    "tool calls require an assistant-role message".to_string(),
                ))]
            }
        }
    }
    let mut object = Map::new();
    object.insert("role".to_string(), Value::String(role.to_string()));
    object.insert(
        "content".to_string(),
        if !images.is_empty() {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(json!({"type":"text","text":text}));
            }
            for image in &images {
                parts.push(json!({"type":"image_url","image_url":{"url":image}}));
            }
            Value::Array(parts)
        } else if text.is_empty() && !calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !calls.is_empty() {
        object.insert("tool_calls".to_string(), Value::Array(calls));
    }
    vec![Ok(Value::Object(object))]
}

/// `pub(crate)`: reused directly (not mocked) by `chat_template_lab.rs`'s
/// non-streaming parse-direction fixtures.
pub(crate) fn parse_openai_response(
    value: &Value,
    request: &CanonicalInferenceRequest,
) -> M3HubResult<CanonicalInferenceResponse> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| M3HubError::Runtime("local response contains no choice".to_string()))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| M3HubError::Runtime("local response choice has no message".to_string()))?;
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(CanonicalContent::Text {
                text: text.to_string(),
            });
        }
    } else if message.get("content").is_some_and(|value| !value.is_null()) {
        return Err(M3HubError::Runtime(
            "local response content is not text".to_string(),
        ));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = required_string(call, "id", "tool call id")?;
            let function = call
                .get("function")
                .ok_or_else(|| M3HubError::Runtime("tool call function is missing".to_string()))?;
            let name = required_string(function, "name", "tool call name")?;
            if !request_offers_tool(request, name) {
                return Err(M3HubError::Runtime(format!(
                    "local response called tool \"{name}\" that was not offered in this request"
                )));
            }
            let arguments = required_string(function, "arguments", "tool call arguments")?;
            let input: Value = serde_json::from_str(arguments).map_err(|error| {
                M3HubError::Runtime(format!("tool call arguments are not JSON: {error}"))
            })?;
            if !input.is_object() {
                return Err(M3HubError::Runtime(
                    "tool call arguments must decode to an object".to_string(),
                ));
            }
            content.push(CanonicalContent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            });
        }
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&request.model)
        .to_string();
    if model != request.model {
        return Err(M3HubError::Runtime(
            "local response model differs from the requested model".to_string(),
        ));
    }
    Ok(CanonicalInferenceResponse {
        response_id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&request.request_id)
            .to_string(),
        model,
        content,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string(),
        usage: parse_usage(value),
        created_at_seconds: value
            .get("created")
            .and_then(Value::as_u64)
            .unwrap_or(now_seconds()?),
    })
}

fn parse_openai_embeddings_response(
    value: &Value,
    request: &CanonicalEmbeddingRequest,
) -> M3HubResult<CanonicalEmbeddingResponse> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| M3HubError::Runtime("local embeddings response has no data".to_string()))?;
    if data.len() != request.input.len() {
        return Err(M3HubError::Runtime(format!(
            "local embeddings endpoint returned {} vectors for {} inputs",
            data.len(),
            request.input.len()
        )));
    }
    let mut items = Vec::with_capacity(data.len());
    for (expected_index, datum) in data.iter().enumerate() {
        let index = datum
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(expected_index as u64) as usize;
        let embedding = datum
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                M3HubError::Runtime("embedding datum has no embedding array".to_string())
            })?
            .iter()
            .map(|component| {
                component.as_f64().map(|value| value as f32).ok_or_else(|| {
                    M3HubError::Runtime("embedding component is not numeric".to_string())
                })
            })
            .collect::<M3HubResult<Vec<f32>>>()?;
        items.push(CanonicalEmbeddingDatum { index, embedding });
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&request.model)
        .to_string();
    Ok(CanonicalEmbeddingResponse {
        model,
        data: items,
        usage: parse_usage(value),
    })
}

fn required_string<'a>(value: &'a Value, key: &str, label: &str) -> M3HubResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| M3HubError::Runtime(format!("{label} is missing")))
}

/// Reads token accounting off a response (or a final stream chunk) root.
///
/// Takes the root rather than the `usage` object because the prompt-cache
/// measurement does not always live inside `usage`: llama-server reports it as
/// `timings.cache_n`, and on a streamed response it sends `timings` in the final
/// chunk *without* a `usage` object at all. Reading only `usage` there would
/// report a stream as having consumed zero tokens.
fn parse_usage(root: &Value) -> CanonicalUsage {
    let usage = root.get("usage");
    let timings = root.get("timings");
    // `cache_n` + `prompt_n` is llama-server's own decomposition of the prompt:
    // reused from the cache, and actually evaluated. Their sum is the prompt
    // length, which is why it can stand in for `prompt_tokens`.
    let reused = timings
        .and_then(|timings| timings.get("cache_n"))
        .and_then(Value::as_u64);
    let evaluated = timings
        .and_then(|timings| timings.get("prompt_n"))
        .and_then(Value::as_u64);
    let input_tokens = usage
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| match (reused, evaluated) {
            (Some(reused), Some(evaluated)) => Some(reused.saturating_add(evaluated)),
            _ => None,
        })
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            timings
                .and_then(|timings| timings.get("predicted_n"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let cached_input_tokens = reused
        .or_else(|| {
            usage
                .and_then(|usage| usage.get("prompt_tokens_details"))
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        // A runtime cannot have reused more prompt than the prompt had. Clamping
        // rather than trusting keeps one malformed response from producing a hit
        // rate above 1 for every process it is later summed into.
        .map(|reused| reused.min(input_tokens));
    CanonicalUsage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
    }
}

/// Charges one completion's measured prompt-cache split to the process that ran
/// it (roadmap K11), when the runtime reported one and a process owns the scope.
///
/// A no-op in both of the cases that are not a measurement: a runtime that
/// reports no reuse figure (`cached_input_tokens` is `None` — Ollama and MLX), and
/// a completion outside any process scope. Neither writes a zero, because a zero
/// in this column claims the runtime measured no reuse.
fn note_measured_reuse(usage: &CanonicalUsage) {
    let Some(reused) = usage.cached_input_tokens else {
        return;
    };
    let Some(process) = crate::run_scope::current_process() else {
        return;
    };
    process.note_context_reuse(reused, usage.input_tokens.saturating_sub(reused));
}

async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
    cancellation: &CancellationToken,
    context: &M3OperationContext,
) -> M3HubResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(M3HubError::Runtime(
            "local inference response exceeds the byte limit".to_string(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::select! {
        _ = context.cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "read local inference response".to_string() }),
        _ = cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "read local inference response".to_string() }),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| M3HubError::Transport(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(M3HubError::Runtime(
                "local inference response exceeds the byte limit".to_string(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// `pub(crate)`: constructed via `Default` and driven through
/// `ingest_sse_line`/`finish` directly by `chat_template_lab.rs`'s streaming
/// fixtures — the same struct real SSE responses are parsed with.
pub(crate) struct OpenAiStreamState {
    response_id: Option<String>,
    model: Option<String>,
    created: Option<u64>,
    next_index: usize,
    text_index: Option<usize>,
    tools: BTreeMap<u64, OpenAiStreamTool>,
    finish_reason: Option<String>,
    usage: CanonicalUsage,
    saw_done: bool,
}

impl Default for OpenAiStreamState {
    fn default() -> Self {
        Self {
            response_id: None,
            model: None,
            created: None,
            next_index: 0,
            text_index: None,
            tools: BTreeMap::new(),
            finish_reason: None,
            usage: CanonicalUsage::default(),
            saw_done: false,
        }
    }
}

struct OpenAiStreamTool {
    content_index: usize,
    call_id: String,
    name: String,
    /// Argument text not yet flushed downstream as a delta event.
    pending_arguments: String,
    /// The full argument text seen so far, kept even after `pending_arguments`
    /// is drained, so completion can verify the concatenation is actually
    /// valid, complete JSON rather than trusting that a stream which stopped
    /// sending bytes must have stopped because it was done.
    full_arguments: String,
    started: bool,
}

impl OpenAiStreamState {
    fn ensure_started(
        &mut self,
        chunk: &Value,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        if self.response_id.is_some() {
            return Ok(());
        }
        let response_id = chunk
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&request.request_id)
            .to_string();
        let model = chunk
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_string();
        if model != request.model {
            return Err(M3HubError::Runtime(
                "local stream model differs from the requested model".to_string(),
            ));
        }
        let created = chunk
            .get("created")
            .and_then(Value::as_u64)
            .unwrap_or(now_seconds()?);
        sink.emit(CanonicalStreamEvent::ResponseStart {
            response_id: response_id.clone(),
            model: model.clone(),
            created_at_seconds: created,
        })
        .map_err(M3HubError::Runtime)?;
        self.response_id = Some(response_id);
        self.model = Some(model);
        self.created = Some(created);
        Ok(())
    }

    fn ingest(
        &mut self,
        chunk: &Value,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        self.ensure_started(chunk, request, sink)?;
        // `timings` as well as `usage`: llama-server puts its token accounting
        // (including the prompt-cache measurement) only in `timings` on a
        // streamed response, and sends it on the final chunk.
        if chunk.get("usage").is_some() || chunk.get("timings").is_some() {
            self.usage = parse_usage(chunk);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                let index = match self.text_index {
                    Some(index) => index,
                    None => {
                        let index = self.next_index;
                        self.next_index += 1;
                        self.text_index = Some(index);
                        sink.emit(CanonicalStreamEvent::TextStart { index })
                            .map_err(M3HubError::Runtime)?;
                        index
                    }
                };
                sink.emit(CanonicalStreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                })
                .map_err(M3HubError::Runtime)?;
            }
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let upstream_index = call
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| M3HubError::Runtime("stream tool index is missing".to_string()))?;
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if !id.is_empty()
                    && self.tools.iter().any(|(other_index, other)| {
                        *other_index != upstream_index && other.call_id == id
                    })
                {
                    return Err(M3HubError::Runtime(
                        "stream reused a tool call id across a different index".to_string(),
                    ));
                }
            }
            let tool = self.tools.entry(upstream_index).or_insert_with(|| {
                let content_index = self.next_index;
                self.next_index += 1;
                OpenAiStreamTool {
                    content_index,
                    call_id: String::new(),
                    name: String::new(),
                    pending_arguments: String::new(),
                    full_arguments: String::new(),
                    started: false,
                }
            });
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if tool.started && !id.is_empty() && id != tool.call_id {
                    return Err(M3HubError::Runtime(
                        "stream changed a tool call id after it started".to_string(),
                    ));
                }
                if !id.is_empty() {
                    tool.call_id = id.to_string();
                }
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    if tool.started && !name.is_empty() && name != tool.name {
                        return Err(M3HubError::Runtime(
                            "stream changed a tool name after it started".to_string(),
                        ));
                    }
                    if !tool.started && !name.is_empty() {
                        tool.name.push_str(name);
                    }
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    tool.pending_arguments.push_str(arguments);
                    tool.full_arguments.push_str(arguments);
                }
            }
            if !tool.started && !tool.call_id.is_empty() && !tool.name.is_empty() {
                sink.emit(CanonicalStreamEvent::ToolCallStart {
                    index: tool.content_index,
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                })
                .map_err(M3HubError::Runtime)?;
                tool.started = true;
            }
            if tool.started && !tool.pending_arguments.is_empty() {
                sink.emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                    index: tool.content_index,
                    call_id: tool.call_id.clone(),
                    json_delta: std::mem::take(&mut tool.pending_arguments),
                })
                .map_err(M3HubError::Runtime)?;
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        if self.response_id.is_none() {
            self.ensure_started(&Value::Null, request, sink)?;
        }
        if let Some(index) = self.text_index {
            sink.emit(CanonicalStreamEvent::TextEnd { index })
                .map_err(M3HubError::Runtime)?;
        }
        for tool in self.tools.values() {
            if !tool.started || !tool.pending_arguments.is_empty() {
                return Err(M3HubError::Runtime(
                    "local stream ended with an incomplete tool call".to_string(),
                ));
            }
            // The stream may end (connection drop, truncated response) after
            // some argument fragments were already flushed as deltas but
            // before the JSON they form together is actually complete. Only
            // `pending_arguments` gets drained on each flush, so checking it
            // alone cannot tell a genuinely finished call from one cut off
            // mid-token; re-parse everything seen for this call instead.
            if serde_json::from_str::<Value>(&tool.full_arguments)
                .ok()
                .filter(Value::is_object)
                .is_none()
            {
                return Err(M3HubError::Runtime(
                    "local stream ended with a truncated or invalid tool call".to_string(),
                ));
            }
            if !request_offers_tool(request, &tool.name) {
                return Err(M3HubError::Runtime(format!(
                    "local stream called tool \"{}\" that was not offered in this request",
                    tool.name
                )));
            }
            sink.emit(CanonicalStreamEvent::ToolCallEnd {
                index: tool.content_index,
                call_id: tool.call_id.clone(),
            })
            .map_err(M3HubError::Runtime)?;
        }
        note_measured_reuse(&self.usage);
        sink.emit(CanonicalStreamEvent::ResponseCompleted {
            response_id: self
                .response_id
                .unwrap_or_else(|| request.request_id.clone()),
            finish_reason: self.finish_reason.unwrap_or_else(|| "stop".to_string()),
            usage: self.usage,
        })
        .map_err(M3HubError::Runtime)
    }
}

async fn parse_openai_sse(
    response: reqwest::Response,
    request: &CanonicalInferenceRequest,
    sink: &mut dyn M3CanonicalStreamSink,
    cancellation: &CancellationToken,
    context: &M3OperationContext,
) -> M3HubResult<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut observed = 0_usize;
    let mut state = OpenAiStreamState::default();
    while let Some(chunk) = tokio::select! {
        _ = context.cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "stream local inference".to_string() }),
        _ = cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "stream local inference".to_string() }),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| M3HubError::Transport(error.to_string()))?;
        observed = observed.saturating_add(chunk.len());
        if observed > MAX_INFERENCE_RESPONSE_BYTES {
            return Err(M3HubError::Runtime(
                "local inference stream exceeds the byte limit".to_string(),
            ));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = buffer.drain(..=position).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line)
                .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
            ingest_sse_line(line, request, sink, &mut state)?;
        }
    }
    if !buffer.is_empty() {
        let line = std::str::from_utf8(&buffer)
            .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
        ingest_sse_line(line.trim_end_matches('\r'), request, sink, &mut state)?;
    }
    state.finish(request, sink)
}

/// `pub(crate)`: drives a single synthetic SSE `data:` line through the same
/// state machine a real streamed response uses; `chat_template_lab.rs` calls
/// this directly (no live HTTP response required) to validate the streaming
/// parse direction.
pub(crate) fn ingest_sse_line(
    line: &str,
    request: &CanonicalInferenceRequest,
    sink: &mut dyn M3CanonicalStreamSink,
    state: &mut OpenAiStreamState,
) -> M3HubResult<()> {
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok(());
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Err(M3HubError::Runtime(
            "local stream contains a non-SSE line".to_string(),
        ));
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        state.saw_done = true;
        return Ok(());
    }
    if state.saw_done {
        return Err(M3HubError::Runtime(
            "local stream emitted data after [DONE]".to_string(),
        ));
    }
    let value: Value = serde_json::from_str(data)?;
    state.ingest(&value, request, sink)
}

// Process/runtime construction and the public constructor follow below.

struct ManagedChildRecord {
    handle: ManagedProcessHandle,
    child: Child,
    log_path: PathBuf,
}

/// Structured, shell-free child-process controller for managed llama.cpp.
/// Children are killed on controller drop, and every launch is readiness-
/// checked on the exact loopback port before it is published to the adapter.
pub struct SystemManagedProcessController {
    log_root: PathBuf,
    children: Mutex<BTreeMap<String, ManagedChildRecord>>,
    mutation: tokio::sync::Mutex<()>,
}

impl SystemManagedProcessController {
    pub fn new(log_root: impl AsRef<Path>) -> M3HubResult<Self> {
        let log_root = log_root.as_ref().to_path_buf();
        ensure_private_directory(&log_root)?;
        Ok(Self {
            log_root,
            children: Mutex::new(BTreeMap::new()),
            mutation: tokio::sync::Mutex::new(()),
        })
    }

    fn runtime_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ManagedChildRecord>>, RuntimeAdapterError>
    {
        self.children
            .lock()
            .map_err(|_| RuntimeAdapterError::LockPoisoned)
    }

    fn owned_port(&self, port: u16) -> Result<Option<PortOwnership>, RuntimeAdapterError> {
        let mut children = self.runtime_lock()?;
        for record in children.values_mut() {
            if record.handle.port != port {
                continue;
            }
            match record.child.try_wait() {
                Ok(None) => {
                    return Ok(Some(PortOwnership {
                        port,
                        owner_id: record.handle.process_id.clone(),
                        runtime: Some(RuntimeKind::LlamaCpp),
                        ownership: ResidencyOwnership::AppManaged,
                    }))
                }
                Ok(Some(_)) => continue,
                Err(error) => {
                    return Err(RuntimeAdapterError::Controller {
                        operation: "inspect managed process".to_string(),
                        message: error.to_string(),
                    })
                }
            }
        }
        Ok(None)
    }

    async fn loopback_port_reachable(port: u16) -> bool {
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    fn create_log_file(&self, process_id: &str) -> Result<(PathBuf, File), RuntimeAdapterError> {
        let path = self.log_root.join(format!("{process_id}.log"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .map_err(|error| RuntimeAdapterError::Controller {
                operation: "create managed runtime log".to_string(),
                message: error.to_string(),
            })?;
        Ok((path, file))
    }

    /// Kills, reaps, and unregisters every child launched by this exact
    /// controller, then verifies its loopback ports are no longer listening.
    /// This is synchronous by design so it remains usable in Tauri's final
    /// `RunEvent::Exit` callback after async task scheduling has stopped.
    pub fn shutdown_all_blocking(&self, timeout: Duration) -> Result<usize, String> {
        let mut records = {
            let mut children = self
                .children
                .lock()
                .map_err(|_| "managed runtime process lock is poisoned".to_string())?;
            std::mem::take(&mut *children)
                .into_values()
                .collect::<Vec<_>>()
        };
        let count = records.len();
        let ports = records
            .iter()
            .map(|record| record.handle.port)
            .collect::<BTreeSet<_>>();
        let mut errors = Vec::new();
        for record in &mut records {
            match record.child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = record.child.start_kill() {
                        errors.push(format!(
                            "kill managed runtime {}: {error}",
                            record.handle.process_id
                        ));
                    }
                }
                Err(error) => errors.push(format!(
                    "inspect managed runtime {}: {error}",
                    record.handle.process_id
                )),
            }
        }

        let deadline = std::time::Instant::now() + timeout;
        while !records.is_empty() && std::time::Instant::now() < deadline {
            records.retain_mut(|record| match record.child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(error) => {
                    errors.push(format!(
                        "reap managed runtime {}: {error}",
                        record.handle.process_id
                    ));
                    false
                }
            });
            if !records.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        for record in &mut records {
            let _ = record.child.start_kill();
            errors.push(format!(
                "managed runtime {} did not exit before the shutdown deadline",
                record.handle.process_id
            ));
        }

        for port in ports {
            while std::time::Instant::now() < deadline
                && std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_millis(20),
                )
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_millis(20),
            )
            .is_ok()
            {
                errors.push(format!(
                    "managed runtime loopback port {port} remained open after shutdown"
                ));
            }
        }

        if errors.is_empty() {
            Ok(count)
        } else {
            Err(errors.join("; "))
        }
    }
}

impl M3OwnedProcessShutdown for SystemManagedProcessController {
    fn shutdown_all_blocking(&self, timeout: Duration) -> Result<usize, String> {
        SystemManagedProcessController::shutdown_all_blocking(self, timeout)
    }
}

impl Drop for SystemManagedProcessController {
    fn drop(&mut self) {
        let _ = self.shutdown_all_blocking(Duration::from_secs(2));
    }
}

impl ManagedProcessController for SystemManagedProcessController {
    fn port_owner<'a>(
        &'a self,
        port: u16,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Option<PortOwnership>> {
        Box::pin(async move {
            context.preflight("inspect managed runtime port")?;
            if let Some(owner) = self.owned_port(port)? {
                return Ok(Some(owner));
            }
            if Self::loopback_port_reachable(port).await {
                Ok(Some(PortOwnership {
                    port,
                    owner_id: format!("external-loopback-port-{port}"),
                    runtime: None,
                    ownership: ResidencyOwnership::PreExisting,
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn launch<'a>(
        &'a self,
        spec: ManagedProcessSpec,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessHandle> {
        Box::pin(async move {
            context.preflight("launch managed llama.cpp")?;
            spec.validate(context.limits.max_config_bytes)?;
            verify_executable(&spec.program)?;
            let _mutation = self.mutation.lock().await;
            if let Some(owner) = self.port_owner(spec.port, context).await? {
                return Err(RuntimeAdapterError::PortCollision {
                    port: spec.port,
                    owner_id: owner.owner_id,
                });
            }
            let process_id = format!("{}-{}", spec.runtime_id, Uuid::new_v4());
            let (log_path, stdout) = self.create_log_file(&process_id)?;
            let stderr = stdout
                .try_clone()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "clone managed runtime log".to_string(),
                    message: error.to_string(),
                })?;
            let mut command = tokio::process::Command::new(&spec.program);
            command
                .args(&spec.args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .kill_on_drop(true);
            let mut child = command
                .spawn()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "spawn managed llama.cpp".to_string(),
                    message: error.to_string(),
                })?;
            let handle = ManagedProcessHandle {
                process_id: process_id.clone(),
                os_pid: child.id(),
                port: spec.port,
                started_at_ms: now_ms().map_err(|error| RuntimeAdapterError::Controller {
                    operation: "timestamp managed llama.cpp".to_string(),
                    message: error.to_string(),
                })?,
            };
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(context.limits.timeout_ms.min(60_000));
            loop {
                context.preflight("wait for managed llama.cpp readiness")?;
                match child
                    .try_wait()
                    .map_err(|error| RuntimeAdapterError::Controller {
                        operation: "inspect launched llama.cpp".to_string(),
                        message: error.to_string(),
                    })? {
                    Some(status) => {
                        return Err(RuntimeAdapterError::Controller {
                            operation: "wait for managed llama.cpp readiness".to_string(),
                            message: format!("process exited before readiness: {status}"),
                        })
                    }
                    None if Self::loopback_port_reachable(spec.port).await => break,
                    None if tokio::time::Instant::now() >= deadline => {
                        let _ = child.kill().await;
                        return Err(RuntimeAdapterError::Timeout {
                            operation: "wait for managed llama.cpp readiness".to_string(),
                            timeout_ms: context.limits.timeout_ms.min(60_000),
                        });
                    }
                    None => {
                        tokio::select! {
                            _ = context.cancellation.cancelled() => {
                                let _ = child.kill().await;
                                return Err(RuntimeAdapterError::Cancelled { operation: "wait for managed llama.cpp readiness".to_string() });
                            }
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        }
                    }
                }
            }
            self.runtime_lock()?.insert(
                process_id,
                ManagedChildRecord {
                    handle: handle.clone(),
                    child,
                    log_path,
                },
            );
            Ok(handle)
        })
    }

    fn inspect<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessStatus> {
        Box::pin(async move {
            context.preflight("inspect managed llama.cpp")?;
            let mut children = self.runtime_lock()?;
            let Some(record) = children.get_mut(&handle.process_id) else {
                return Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Exited,
                    exit_code: None,
                    message: Some("owned process is no longer registered".to_string()),
                });
            };
            if record.handle != *handle {
                return Err(RuntimeAdapterError::Controller {
                    operation: "inspect managed llama.cpp".to_string(),
                    message: "process handle does not match the owned record".to_string(),
                });
            }
            match record.child.try_wait() {
                Ok(None) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Ready,
                    exit_code: None,
                    message: None,
                }),
                Ok(Some(status)) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Exited,
                    exit_code: status.code(),
                    message: Some(format!("managed llama.cpp exited: {status}")),
                }),
                Err(error) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Failed,
                    exit_code: None,
                    message: Some(error.to_string()),
                }),
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            context.preflight("terminate managed llama.cpp")?;
            let _mutation = self.mutation.lock().await;
            let record = self.runtime_lock()?.remove(&handle.process_id);
            let Some(mut record) = record else {
                return Ok(());
            };
            if record.handle != *handle {
                return Err(RuntimeAdapterError::Controller {
                    operation: "terminate managed llama.cpp".to_string(),
                    message: "process handle does not match the owned record".to_string(),
                });
            }
            if record
                .child
                .try_wait()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "inspect process before termination".to_string(),
                    message: error.to_string(),
                })?
                .is_none()
            {
                record
                    .child
                    .kill()
                    .await
                    .map_err(|error| RuntimeAdapterError::Controller {
                        operation: "kill managed llama.cpp".to_string(),
                        message: error.to_string(),
                    })?;
            }
            Ok(())
        })
    }

    fn tail_logs<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        max_bytes: usize,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedLogChunk> {
        Box::pin(async move {
            context.preflight("tail managed llama.cpp logs")?;
            if max_bytes == 0 || max_bytes > context.limits.max_log_bytes {
                return Err(RuntimeAdapterError::LogTooLarge {
                    limit: context.limits.max_log_bytes,
                    actual: max_bytes,
                });
            }
            let path = self
                .runtime_lock()?
                .get(&handle.process_id)
                .filter(|record| record.handle == *handle)
                .map(|record| record.log_path.clone())
                .ok_or_else(|| RuntimeAdapterError::Controller {
                    operation: "tail managed llama.cpp logs".to_string(),
                    message: "owned process is no longer registered".to_string(),
                })?;
            read_log_tail(&path, max_bytes)
        })
    }
}

fn verify_executable(path: &Path) -> Result<(), RuntimeAdapterError> {
    if !path.is_absolute() {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable must be absolute".to_string(),
        });
    }
    // Resolve symlinks first: PATH-discovered binaries (e.g. Homebrew's
    // `/opt/homebrew/bin/llama-server`) are routinely symlinks into a
    // versioned Cellar path. The tamper check below must apply to the real
    // target file, not the link itself, or every symlinked install fails
    // closed here.
    let real_path = fs::canonicalize(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "resolve managed executable".to_string(),
        message: error.to_string(),
    })?;
    let metadata =
        fs::symlink_metadata(&real_path).map_err(|error| RuntimeAdapterError::Controller {
            operation: "inspect managed executable".to_string(),
            message: error.to_string(),
        })?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable must be a real regular file".to_string(),
        });
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable is not executable".to_string(),
        });
    }
    Ok(())
}

fn read_log_tail(path: &Path, max_bytes: usize) -> Result<ManagedLogChunk, RuntimeAdapterError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "inspect managed runtime log".to_string(),
        message: error.to_string(),
    })?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeAdapterError::Controller {
            operation: "inspect managed runtime log".to_string(),
            message: "log path is not a regular file".to_string(),
        });
    }
    let total = metadata.len();
    let start = total.saturating_sub(max_bytes as u64);
    let mut file = File::open(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "open managed runtime log".to_string(),
        message: error.to_string(),
    })?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| RuntimeAdapterError::Controller {
            operation: "seek managed runtime log".to_string(),
            message: error.to_string(),
        })?;
    let mut bytes = Vec::with_capacity((total - start) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| RuntimeAdapterError::Controller {
            operation: "read managed runtime log".to_string(),
            message: error.to_string(),
        })?;
    Ok(ManagedLogChunk {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: start > 0,
    })
}

fn ensure_private_directory(path: &Path) -> M3HubResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(M3HubError::State(format!(
                "{} is not a real directory",
                path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| M3HubError::Io {
                operation: "create private M3 directory",
                path: path.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect private M3 directory",
                path: path.to_path_buf(),
                source,
            })
        }
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        M3HubError::Io {
            operation: "secure private M3 directory",
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct ProductionMlxSignatureVerifier;

#[cfg(target_os = "macos")]
impl MlxSignatureVerifier for ProductionMlxSignatureVerifier {
    fn verify(
        &self,
        algorithm: &str,
        key_id: &str,
        signed_payload: &[u8],
        signature_bytes: &[u8],
    ) -> Result<(), String> {
        if algorithm != "ed25519" || key_id != MLX_RELEASE_KEY_ID {
            return Err("MLX package is not signed by the pinned release key".to_string());
        }
        let public_key = decode_hex(MLX_RELEASE_PUBLIC_KEY_HEX)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
            .verify(signed_payload, signature_bytes)
            .map_err(|_| "MLX package Ed25519 signature is invalid".to_string())
    }
}

#[cfg(target_os = "macos")]
fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("pinned key is not valid hexadecimal".to_string());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .map_err(|_| "pinned key is not UTF-8 hexadecimal".to_string())
                .and_then(|pair| {
                    u8::from_str_radix(pair, 16)
                        .map_err(|_| "pinned key is not valid hexadecimal".to_string())
                })
        })
        .collect()
}

/// Controller for the verified app-private MLX service package. The package
/// process is supervised by the same structured process boundary as
/// llama.cpp; generation uses a loopback-only SSE endpoint whose data values
/// are the versioned [`MlxStreamEvent`] schema.
#[cfg(target_os = "macos")]
struct ProductionMlxServiceController {
    process: Arc<SystemManagedProcessController>,
    handles: Mutex<BTreeMap<String, ManagedProcessHandle>>,
    cancellations: Mutex<BTreeMap<String, CancellationToken>>,
    generated_tokens: AtomicU64,
    client: reqwest::Client,
}

#[cfg(target_os = "macos")]
impl ProductionMlxServiceController {
    fn new(process: Arc<SystemManagedProcessController>) -> M3HubResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            process,
            handles: Mutex::new(BTreeMap::new()),
            cancellations: Mutex::new(BTreeMap::new()),
            generated_tokens: AtomicU64::new(0),
            client,
        })
    }

    fn runtime_context(context: &MlxOperationContext) -> RuntimeOperationContext {
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        RuntimeOperationContext::new(limits, context.cancellation.clone())
    }

    fn controller_error(operation: &str, error: impl std::fmt::Display) -> MlxError {
        MlxError::Controller {
            operation: operation.to_string(),
            message: error.to_string(),
        }
    }
}

#[cfg(target_os = "macos")]
impl MlxServiceController for ProductionMlxServiceController {
    fn port_owner<'a>(&'a self, port: u16) -> MlxFuture<'a, Option<String>> {
        Box::pin(async move {
            let context = RuntimeOperationContext::default();
            self.process
                .port_owner(port, &context)
                .await
                .map(|owner| owner.map(|owner| owner.owner_id))
                .map_err(|error| Self::controller_error("inspect MLX port", error))
        })
    }

    fn launch<'a>(
        &'a self,
        spec: MlxLaunchSpec,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessHandle> {
        Box::pin(async move {
            let runtime_context = Self::runtime_context(context);
            let handle = self
                .process
                .launch(
                    ManagedProcessSpec {
                        runtime_id: spec.runtime_id,
                        program: spec.program,
                        args: spec.args,
                        port: spec.port,
                    },
                    &runtime_context,
                )
                .await
                .map_err(|error| Self::controller_error("launch MLX service", error))?;
            lock(&self.handles)
                .map_err(|error| Self::controller_error("record MLX process", error))?
                .insert(handle.process_id.clone(), handle.clone());
            Ok(MlxProcessHandle {
                process_id: handle.process_id,
                os_pid: handle.os_pid,
                port: handle.port,
                model_id: spec.model_id,
                started_at_ms: handle.started_at_ms,
            })
        })
    }

    fn inspect<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics> {
        Box::pin(async move {
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("read MLX process", error))?
                .get(&handle.process_id)
                .cloned();
            let process_alive = if let Some(managed) = managed {
                let runtime_context = Self::runtime_context(context);
                matches!(
                    self.process
                        .inspect(&managed, &runtime_context)
                        .await
                        .map_err(|error| Self::controller_error("inspect MLX service", error))?
                        .state,
                    ManagedProcessState::Starting | ManagedProcessState::Ready
                )
            } else {
                false
            };
            let resident_memory_bytes = if process_alive {
                handle
                    .os_pid
                    .and_then(process_resident_memory_bytes)
                    .unwrap_or(0)
            } else {
                0
            };
            Ok(MlxProcessMetrics {
                process_alive,
                resident_memory_bytes,
                unified_memory_bytes: if cfg!(target_os = "macos") {
                    resident_memory_bytes
                } else {
                    0
                },
                active_requests: lock(&self.cancellations)
                    .map_err(|error| Self::controller_error("read MLX requests", error))?
                    .len() as u64,
                generated_tokens: self.generated_tokens.load(Ordering::Relaxed),
                tokens_per_second: None,
                sampled_at_ms: now_ms()
                    .map_err(|error| Self::controller_error("timestamp MLX metrics", error))?,
            })
        })
    }

    fn stream<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        request: &'a MlxGenerationRequest,
        sink: &'a mut dyn MlxStreamSink,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxGenerationSummary> {
        Box::pin(async move {
            let cancellation = CancellationToken::new();
            {
                let mut cancellations = lock(&self.cancellations)
                    .map_err(|error| Self::controller_error("register MLX request", error))?;
                if cancellations.contains_key(&request.request_id) {
                    return Err(MlxError::RequestAlreadyRunning(request.request_id.clone()));
                }
                cancellations.insert(request.request_id.clone(), cancellation.clone());
            }
            let result = async {
                let response = tokio::select! {
                    _ = context.cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    _ = cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    response = crate::egress::send(self.client.post(format!("http://127.0.0.1:{}/v1/generate", handle.port)).json(request)) => {
                        response.map_err(|error| Self::controller_error("start MLX stream", error))?
                    }
                };
                if !response.status().is_success() {
                    return Err(Self::controller_error(
                        "start MLX stream",
                        format!("HTTP {}", response.status()),
                    ));
                }
                let mut bytes = response.bytes_stream();
                let mut buffer = Vec::new();
                let mut observed = 0_usize;
                let mut completed = None;
                let mut used_tool = false;
                while let Some(chunk) = tokio::select! {
                    _ = context.cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    _ = cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    chunk = bytes.next() => chunk,
                } {
                    let chunk = chunk.map_err(|error| Self::controller_error("read MLX stream", error))?;
                    observed = observed.saturating_add(chunk.len());
                    if observed > 64 * 1024 * 1024 {
                        return Err(MlxError::Limit {
                            name: "MLX service stream bytes",
                            observed: observed as u64,
                            max: 64 * 1024 * 1024,
                        });
                    }
                    buffer.extend_from_slice(&chunk);
                    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                        let mut line = buffer.drain(..=position).collect::<Vec<_>>();
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        ingest_mlx_service_line(
                            &line,
                            sink,
                            &mut completed,
                            &mut used_tool,
                        )?;
                    }
                }
                if !buffer.is_empty() {
                    ingest_mlx_service_line(&buffer, sink, &mut completed, &mut used_tool)?;
                }
                let (input_tokens, output_tokens) = completed.ok_or_else(|| {
                    MlxError::StreamProtocol(
                        "MLX service stream ended without a completed event".to_string(),
                    )
                })?;
                Ok(MlxGenerationSummary {
                    request_id: request.request_id.clone(),
                    input_tokens,
                    output_tokens,
                    finish_reason: if used_tool { "tool_use" } else { "stop" }.to_string(),
                })
            }
            .await;
            if let Ok(summary) = &result {
                self.generated_tokens
                    .fetch_add(summary.output_tokens, Ordering::Relaxed);
            }
            if let Ok(mut cancellations) = self.cancellations.lock() {
                cancellations.remove(&request.request_id);
            }
            result
        })
    }

    fn cancel<'a>(
        &'a self,
        _handle: &'a MlxProcessHandle,
        request_id: &'a str,
    ) -> MlxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(cancellation) = lock(&self.cancellations)
                .map_err(|error| Self::controller_error("cancel MLX request", error))?
                .get(request_id)
            {
                cancellation.cancel();
            }
            Ok(())
        })
    }

    fn terminate_and_wait<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        _timeout_ms: u64,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics> {
        Box::pin(async move {
            for cancellation in lock(&self.cancellations)
                .map_err(|error| Self::controller_error("cancel MLX requests", error))?
                .values()
            {
                cancellation.cancel();
            }
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("remove MLX process", error))?
                .remove(&handle.process_id);
            if let Some(managed) = managed {
                let runtime_context = Self::runtime_context(context);
                self.process
                    .terminate(&managed, &runtime_context)
                    .await
                    .map_err(|error| Self::controller_error("terminate MLX service", error))?;
            }
            Ok(MlxProcessMetrics {
                process_alive: false,
                resident_memory_bytes: 0,
                unified_memory_bytes: 0,
                active_requests: 0,
                generated_tokens: 0,
                tokens_per_second: None,
                sampled_at_ms: now_ms()
                    .map_err(|error| Self::controller_error("timestamp MLX unload", error))?,
            })
        })
    }

    fn tail_logs<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        max_bytes: usize,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, String> {
        Box::pin(async move {
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("read MLX process", error))?
                .get(&handle.process_id)
                .cloned()
                .ok_or(MlxError::NotRunning)?;
            let runtime_context = Self::runtime_context(context);
            self.process
                .tail_logs(&managed, max_bytes, &runtime_context)
                .await
                .map(|logs| logs.text)
                .map_err(|error| Self::controller_error("tail MLX logs", error))
        })
    }
}

#[cfg(target_os = "macos")]
fn ingest_mlx_service_line(
    raw_line: &[u8],
    sink: &mut dyn MlxStreamSink,
    completed: &mut Option<(u64, u64)>,
    used_tool: &mut bool,
) -> Result<(), MlxError> {
    let line = std::str::from_utf8(raw_line)
        .map_err(|_| MlxError::StreamProtocol("MLX service stream is not UTF-8".to_string()))?
        .trim();
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok(());
    }
    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if data == "[DONE]" {
        return Ok(());
    }
    let event: MlxStreamEvent = serde_json::from_str(data)?;
    if matches!(event, MlxStreamEvent::ToolCallStart { .. }) {
        *used_tool = true;
    }
    if let MlxStreamEvent::Completed {
        input_tokens,
        output_tokens,
    } = &event
    {
        *completed = Some((*input_tokens, *output_tokens));
    }
    sink.emit(event).map_err(MlxError::StreamProtocol)
}

// MLX metrics only; macOS is always unix, so there is no non-unix variant to
// keep alive here.
#[cfg(target_os = "macos")]
fn process_resident_memory_bytes(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1_024)
}

#[cfg(target_os = "macos")]
struct ProductionMlxComponents {
    installer: Arc<MlxPackageInstaller>,
    controller: Arc<ProductionMlxServiceController>,
}

struct ProductionRuntimeFactory {
    root: PathBuf,
    // Only the MLX driver needs a clock; the other drivers take their timestamps
    // from the runtime adapter they wrap.
    #[cfg(target_os = "macos")]
    clock: Arc<dyn M3Clock>,
    process_controller: Arc<SystemManagedProcessController>,
}

impl ProductionRuntimeFactory {
    fn build_all(
        &self,
        installed: &[M3InstalledModelView],
    ) -> M3HubResult<Vec<Arc<dyn M3RuntimeDriver>>> {
        let hardware = SystemM3HardwareProbe.snapshot()?;
        let mut drivers = vec![build_ollama_driver(hardware.platform.clone())?];
        drivers.extend(self.build_managed(installed, hardware.platform)?);
        Ok(drivers)
    }

    fn build_managed(
        &self,
        installed: &[M3InstalledModelView],
        platform: PlatformCapabilities,
    ) -> M3HubResult<Vec<Arc<dyn M3RuntimeDriver>>> {
        let mut drivers = Vec::new();
        if let Some(binary) = find_production_llama_binary(&self.root)? {
            let models = runtime_models(installed, M3RuntimeKind::LlamaCpp)?;
            let structured_output_models = installed
                .iter()
                .filter(|model| {
                    model.runtime == M3RuntimeKind::LlamaCpp && model.capabilities.structured_output
                })
                .map(|model| model.model_id.clone())
                .collect();
            let adapter: Arc<dyn RuntimeAdapter> = Arc::new(
                ManagedLlamaCppAdapter::new(
                    LLAMA_RUNTIME_ID,
                    LLAMA_ENDPOINT,
                    binary,
                    LLAMA_PORT,
                    self.process_controller.clone(),
                    models,
                    platform,
                )
                .map_err(runtime_error)?,
            );
            let inference: Arc<dyn M3InferenceEngine> =
                Arc::new(CapabilityCheckedInferenceEngine {
                    adapter: adapter.clone(),
                    inner: Arc::new(OpenAiCompatibleM3InferenceEngine::new(LLAMA_ENDPOINT)?),
                    structured_output_models,
                });
            drivers.push(Arc::new(RuntimeAdapterM3Driver::new(adapter, inference)?)
                as Arc<dyn M3RuntimeDriver>);
        }
        #[cfg(target_os = "macos")]
        if let Some(mlx) = production_mlx_components(&self.root, self.process_controller.clone())? {
            let models = mlx_models(installed)?;
            let adapter = Arc::new(
                MlxRuntimeAdapter::new(
                    MlxRuntimeConfig::default(),
                    Arc::new(CurrentHostMlxProbe),
                    mlx.installer,
                    mlx.controller,
                    models,
                )
                .map_err(|error| M3HubError::Runtime(error.to_string()))?,
            );
            drivers.push(
                Arc::new(MlxM3Driver::new("mlx", adapter, self.clock.clone())?)
                    as Arc<dyn M3RuntimeDriver>,
            );
        }
        Ok(drivers)
    }
}

struct ProductionRuntimeReconciler {
    factory: Arc<ProductionRuntimeFactory>,
    current: Mutex<ProductionRuntimeSnapshot>,
}

struct ProductionRuntimeSnapshot {
    inventory_signature: Vec<(String, String)>,
    drivers: Vec<Arc<dyn M3RuntimeDriver>>,
}

impl ProductionRuntimeReconciler {
    fn new(
        factory: Arc<ProductionRuntimeFactory>,
        installed: &[M3InstalledModelView],
        current: &[Arc<dyn M3RuntimeDriver>],
    ) -> Self {
        Self {
            factory,
            current: Mutex::new(ProductionRuntimeSnapshot {
                inventory_signature: runtime_inventory_signature(installed),
                drivers: current.to_vec(),
            }),
        }
    }
}

fn runtime_inventory_signature(installed: &[M3InstalledModelView]) -> Vec<(String, String)> {
    let mut signature = installed
        .iter()
        .map(|model| (model.asset_id.clone(), model.active_version_key.clone()))
        .collect::<Vec<_>>();
    signature.sort();
    signature
}

impl M3RuntimeReconciler for ProductionRuntimeReconciler {
    fn reconcile<'a>(
        &'a self,
        installed: &'a [M3InstalledModelView],
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<Arc<dyn M3RuntimeDriver>>> {
        Box::pin(async move {
            let (current_signature, current_drivers) = {
                let current = lock(&self.current)?;
                (current.inventory_signature.clone(), current.drivers.clone())
            };
            let requested_signature = runtime_inventory_signature(installed);
            let mut managed_running = false;
            for driver in current_drivers
                .iter()
                .filter(|driver| driver.descriptor().kind != M3RuntimeKind::Ollama)
            {
                let running = match driver.status(context).await? {
                    M3RuntimeStatusView::Adapter { running_models, .. } => {
                        !running_models.is_empty()
                    }
                    #[cfg(target_os = "macos")]
                    M3RuntimeStatusView::Mlx { status } => {
                        matches!(status, crate::mlx_runtime::MlxRuntimeStatus::Running { .. })
                    }
                };
                if running {
                    managed_running = true;
                    break;
                }
            }
            if managed_running {
                if requested_signature != current_signature {
                    return Err(M3HubError::Conflict(
                        "managed runtime inventory cannot change while a model is loaded"
                            .to_string(),
                    ));
                }
                // A factory refresh while a model is resident is safe but
                // deliberately deferred: replacing the driver would lose its
                // ownership handle. Return the live drivers unchanged.
                return Ok(current_drivers);
            }
            let drivers = self.factory.build_all(installed)?;
            *lock(&self.current)? = ProductionRuntimeSnapshot {
                inventory_signature: requested_signature,
                drivers: drivers.clone(),
            };
            Ok(drivers)
        })
    }
}

fn runtime_models(
    installed: &[M3InstalledModelView],
    runtime: M3RuntimeKind,
) -> M3HubResult<Vec<RuntimeModel>> {
    let mut model_ids = BTreeSet::new();
    installed
        .iter()
        .filter(|model| model.runtime == runtime)
        .map(|model| {
            if !model_ids.insert(model.model_id.clone()) {
                return Err(M3HubError::Conflict(format!(
                    "managed runtime cannot expose multiple active variants named {}",
                    model.model_id
                )));
            }
            let version = active_version(model)?;
            Ok(RuntimeModel {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                size_bytes: version.size_bytes,
                local_path: Some(version.artifact_path.clone()),
                digest: Some(version.sha256.clone()),
                modified_at: Some(version.installed_at_ms.to_string()),
                capabilities: ModelCapabilities {
                    chat: model_capabilities(model).chat,
                    embeddings: model_capabilities(model).embeddings,
                    tool_calling: model_capabilities(model).tool_calling,
                    vision: model_capabilities(model).vision,
                },
                metadata: BTreeMap::from([
                    ("assetId".to_string(), model.asset_id.clone()),
                    ("variantId".to_string(), model.variant_id.clone()),
                    ("revision".to_string(), version.revision.clone()),
                ]),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn mlx_models(installed: &[M3InstalledModelView]) -> M3HubResult<Vec<MlxModelRecord>> {
    let mut model_ids = BTreeSet::new();
    installed
        .iter()
        .filter(|model| model.runtime == M3RuntimeKind::Mlx)
        .map(|model| {
            if !model_ids.insert(model.model_id.clone()) {
                return Err(M3HubError::Conflict(format!(
                    "MLX cannot expose multiple active variants named {}",
                    model.model_id
                )));
            }
            let version = active_version(model)?;
            let capabilities = model_capabilities(model);
            Ok(MlxModelRecord {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                local_path: version.artifact_path.clone(),
                size_bytes: version.size_bytes,
                revision: Some(version.revision.clone()),
                capabilities: MlxModelCapabilities {
                    chat: capabilities.chat,
                    tool_calling: capabilities.tool_calling,
                    vision: capabilities.vision,
                    structured_output: capabilities.structured_output,
                },
            })
        })
        .collect()
}

fn active_version(
    model: &M3InstalledModelView,
) -> M3HubResult<&crate::m3_runtime_hub::M3InstalledVersionView> {
    model
        .versions
        .iter()
        .find(|version| version.version_key == model.active_version_key && version.active)
        .ok_or_else(|| {
            M3HubError::State(format!(
                "installed model {} has no active version",
                model.asset_id
            ))
        })
}

fn model_capabilities(model: &M3InstalledModelView) -> &M3ModelCapabilities {
    &model.capabilities
}

fn find_production_llama_binary(root: &Path) -> M3HubResult<Option<PathBuf>> {
    #[cfg(target_os = "windows")]
    let filename = "llama-server.exe";
    #[cfg(not(target_os = "windows"))]
    let filename = "llama-server";
    let managed = root
        .join("runtimes")
        .join("llama")
        .join("current")
        .join(filename);
    match fs::symlink_metadata(&managed) {
        Ok(_) => {
            verify_executable(&managed).map_err(runtime_error)?;
            return Ok(Some(managed));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect app-private llama.cpp runtime",
                path: managed,
                source,
            })
        }
    }
    match crate::llama::find_llama_server_binary() {
        Ok(path) => {
            let path = PathBuf::from(path);
            verify_executable(&path).map_err(runtime_error)?;
            Ok(Some(path))
        }
        Err(_) => Ok(None),
    }
}

/// The one installer every MLX path goes through, install and status alike.
///
/// Factored out so the Tauri install command and the driver that reads
/// `verify_active()` cannot drift onto different roots, verifiers, or limits —
/// three settings where a mismatch would either silently install somewhere
/// nothing reads or reject a package the rest of the app considers valid.
///
/// ponytail: a fresh instance per caller, so the installer's in-process
/// operation lock does not span them. Safe because the on-disk protocol is
/// already atomic — staging directory, rename into place, atomic `active.json`
/// write — and installs are user-initiated and rare. Hand out a shared `Arc`
/// if concurrent installs ever become a real path.
#[cfg(target_os = "macos")]
pub fn production_mlx_installer(root: &Path) -> M3HubResult<Arc<MlxPackageInstaller>> {
    Ok(Arc::new(
        MlxPackageInstaller::new(
            root.join("runtimes").join("mlx"),
            Arc::new(ProductionMlxSignatureVerifier),
            MlxInstallLimits::default(),
        )
        .map_err(|error| M3HubError::Runtime(error.to_string()))?,
    ))
}

/// What an install reports back. Paths stay inside the app's private tree, so
/// only the identity of the package crosses to the UI.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MlxInstalledPackageView {
    pub package_version: String,
    pub manifest_sha256: String,
}

/// Installs a built MLX package directory and makes it the active one.
///
/// `package_directory` is a tree the user points at — the output of
/// `pnpm mlx:package`. Its trustworthiness is not a property of where it came
/// from: the manifest inside it must be signed by the pinned release key, and
/// every file's digest must match, or nothing is written. That is why an
/// arbitrary caller-supplied path is safe here in a way it would not be for,
/// say, a directory of binaries copied into place.
#[cfg(target_os = "macos")]
pub fn install_mlx_package(
    app_data_dir: &Path,
    package_directory: &Path,
) -> M3HubResult<MlxInstalledPackageView> {
    let host = MlxHostCapabilities::current();
    // Derived the same way `build_m3_command_state` derives it, so an install
    // lands in the tree the running driver reads rather than beside it.
    let installer = production_mlx_installer(&app_data_dir.join(M3_DIRECTORY))?;
    let bundle = mlx_runtime::read_package_directory(package_directory, &MlxInstallLimits::default())
        .map_err(|error| M3HubError::Runtime(error.to_string()))?;
    let installed = installer
        .install_and_activate(&bundle, &host)
        .map_err(|error| M3HubError::Runtime(error.to_string()))?;
    Ok(MlxInstalledPackageView {
        package_version: installed.package_version,
        manifest_sha256: installed.manifest_sha256,
    })
}

/// Installs an MLX package from a `.tar.gz` the component hub downloaded.
///
/// This is what makes an artifact feed reach MLX. The component hub already
/// fetches by URL with resume, refuses a body whose length or SHA-256 does not
/// match the catalog entry, and keeps versions and channels — so nothing about
/// downloading is reimplemented here. What was missing is only this: unpack
/// that blob and put it through the signature-verifying installer.
///
/// The extraction goes to a temporary directory that is removed on every path,
/// success or failure. Nothing under it is trusted: `read_package_directory`
/// loads only manifest-declared files and `install_and_activate` re-derives
/// every digest and verifies the publisher signature before publishing a byte.
#[cfg(target_os = "macos")]
pub fn install_mlx_from_artifact(
    app_data_dir: &Path,
    artifact_path: &Path,
) -> M3HubResult<MlxInstalledPackageView> {
    let limits = MlxInstallLimits::default();
    let staging = std::env::temp_dir().join(format!("mlx-unpack-{}", uuid::Uuid::new_v4()));
    let unpacked = (|| {
        mlx_runtime::extract_package_archive(artifact_path, &staging, &limits)?;
        mlx_runtime::read_package_directory(&staging, &limits)
    })();
    let bundle = match unpacked {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(M3HubError::Runtime(error.to_string()));
        }
    };
    let installed = production_mlx_installer(&app_data_dir.join(M3_DIRECTORY))?
        .install_and_activate(&bundle, &MlxHostCapabilities::current())
        .map_err(|error| M3HubError::Runtime(error.to_string()));
    let _ = fs::remove_dir_all(&staging);
    let installed = installed?;
    Ok(MlxInstalledPackageView {
        package_version: installed.package_version,
        manifest_sha256: installed.manifest_sha256,
    })
}

#[cfg(target_os = "macos")]
fn production_mlx_components(
    root: &Path,
    process: Arc<SystemManagedProcessController>,
) -> M3HubResult<Option<ProductionMlxComponents>> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Ok(None);
    }
    let installer = production_mlx_installer(root)?;
    match installer.verify_active() {
        Ok(_) | Err(MlxError::NotInstalled) => Ok(Some(ProductionMlxComponents {
            installer,
            controller: Arc::new(ProductionMlxServiceController::new(process)?),
        })),
        Err(error) => Err(M3HubError::Runtime(format!(
            "verified MLX installation is corrupt: {error}"
        ))),
    }
}

fn build_ollama_driver(platform: PlatformCapabilities) -> M3HubResult<Arc<dyn M3RuntimeDriver>> {
    let transport: Arc<dyn HttpTransport> =
        Arc::new(ReqwestHttpTransport::new().map_err(runtime_error)?);
    let adapter: Arc<dyn RuntimeAdapter> = Arc::new(
        OllamaHttpAdapter::new(
            OLLAMA_RUNTIME_ID,
            OLLAMA_ENDPOINT,
            EndpointPolicy::LoopbackOnly,
            transport,
            platform,
        )
        .map_err(runtime_error)?,
    );
    let inference: Arc<dyn M3InferenceEngine> = Arc::new(CapabilityCheckedInferenceEngine {
        adapter: adapter.clone(),
        inner: Arc::new(OpenAiCompatibleM3InferenceEngine::new(OLLAMA_ENDPOINT)?),
        // The Ollama inventory API does not expose a reliable structured-
        // output capability flag. Fail closed until a richer model card does.
        structured_output_models: BTreeSet::new(),
    });
    Ok(Arc::new(RuntimeAdapterM3Driver::new(adapter, inference)?) as Arc<dyn M3RuntimeDriver>)
}

/// Builds the fully wired production M3 command state below
/// `<app_data_dir>/m3`.
///
/// This constructor performs no model download and starts no runtime. It does
/// validate persisted state, configured catalog origins, the keychain trust
/// root, installed runtime artifacts, and the initial reconciled inventory so
/// a corrupt dependency fails closed before any command is exposed.
pub fn build_m3_command_state(app_data_dir: impl AsRef<Path>) -> M3HubResult<M3CommandState> {
    let app_data_dir = app_data_dir.as_ref();
    if !app_data_dir.is_absolute() {
        return Err(M3HubError::State(
            "Tauri app-data directory must be absolute".to_string(),
        ));
    }
    ensure_private_directory(app_data_dir)?;
    let root = app_data_dir.join(M3_DIRECTORY);
    ensure_private_directory(&root)?;
    let config = M3HubConfig::default();
    let clock: Arc<dyn M3Clock> = Arc::new(SystemM3Clock);
    let hardware: Arc<dyn M3HardwareProbe> = Arc::new(SystemM3HardwareProbe);
    let hardware_snapshot = hardware.snapshot()?;
    let download = Arc::new(ReqwestM3DownloadTransport::new()?);
    let catalogs = load_catalog_sources(&root)?;
    let protector = Arc::new(KeychainLanStateProtector::load_or_create()?);
    let lan_factory = Arc::new(DefaultM3LanAccessFactory::new(
        Arc::new(OsLanEntropy),
        protector,
    ));
    let ollama = build_ollama_driver(hardware_snapshot.platform.clone())?;

    // First load validates the durable store and exposes its exact active
    // model views. The final hub is then created with a matching runtime
    // inventory, avoiding a startup window with stale managed model paths.
    let index_hub = M3RuntimeHub::new(
        &root,
        config.clone(),
        M3RuntimeHubDependencies {
            clock: clock.clone(),
            hardware: hardware.clone(),
            download: download.clone(),
            catalogs: catalogs.clone(),
            runtimes: vec![ollama.clone()],
            runtime_reconciler: None,
            lan_factory: Some(lan_factory.clone()),
        },
    )?;
    let installed = index_hub.list_installed_models()?;
    drop(index_hub);

    let process = Arc::new(SystemManagedProcessController::new(root.join("logs"))?);
    let factory = Arc::new(ProductionRuntimeFactory {
        root: root.clone(),
        #[cfg(target_os = "macos")]
        clock: clock.clone(),
        process_controller: process.clone(),
    });
    let runtimes = factory.build_all(&installed)?;
    let reconciler = Arc::new(ProductionRuntimeReconciler::new(
        factory, &installed, &runtimes,
    ));
    // Runtime components (the llama.cpp/MLX/tokenizer/converter/projector
    // binaries and accelerator-support packages the app itself depends on)
    // are a distinct system from installed models, so they get their own
    // storage root, config, and hub instance rather than sharing the model
    // hub's state.
    let component_root = app_data_dir.join(M3_COMPONENTS_DIRECTORY);
    ensure_private_directory(&component_root)?;
    let component_sources = load_component_sources(&component_root)?;
    let component_config = M3HubConfig {
        schema_version: config.schema_version,
        // Components are small binaries/libraries, not multi-gigabyte model
        // weights, so a much smaller quota is enough headroom.
        storage_quota_bytes: 16 * 1024 * 1024 * 1024,
        storage_reserve_bytes: 256 * 1024 * 1024,
        download_chunk_bytes: config.download_chunk_bytes,
        operation_timeout_ms: config.operation_timeout_ms,
        max_catalog_results: config.max_catalog_results,
    };
    let component_hub = M3ComponentHub::new(
        &component_root,
        component_config,
        M3ComponentHubDependencies {
            clock: clock.clone(),
            download: download.clone(),
            sources: component_sources,
        },
    )?;

    let hub = M3RuntimeHub::new(
        root,
        config,
        M3RuntimeHubDependencies {
            clock,
            hardware,
            download,
            catalogs,
            runtimes,
            runtime_reconciler: Some(reconciler),
            lan_factory: Some(lan_factory),
        },
    )?;
    Ok(M3CommandState::with_owned_processes(
        Arc::new(hub),
        Arc::new(component_hub),
        process,
    ))
}

/// Production wiring for the Model Conversion and Quantization Workbench
/// (ROADMAP.md Phase 8): a storage root separate from the model manifest/blob
/// store, and the real `llama-quantize` backend when it is genuinely found
/// on this machine (see `quantization::find_llama_quantize_binary`),
/// otherwise only the honest `Copy`-only passthrough fallback. No fabricated
/// "always available" real quantizer — see `quantization.rs`'s module doc
/// comment for the full honesty note, mirroring how the Runtime Component
/// Update Channels PR documented its own registry-source honesty.
pub fn build_quantization_command_state(
    app_data_dir: impl AsRef<Path>,
) -> M3HubResult<crate::m3_commands::M3QuantizationCommandState> {
    let app_data_dir = app_data_dir.as_ref();
    if !app_data_dir.is_absolute() {
        return Err(M3HubError::State(
            "Tauri app-data directory must be absolute".to_string(),
        ));
    }
    ensure_private_directory(app_data_dir)?;
    let root = app_data_dir.join("m3-quantization");
    ensure_private_directory(&root)?;

    let mut backends: Vec<Arc<dyn crate::quantization::QuantizationBackend>> = Vec::new();
    if let Some(backend) = crate::quantization::LlamaCppQuantizeBackend::discover() {
        backends.push(Arc::new(backend));
    }
    backends.push(Arc::new(crate::quantization::PassthroughGgufRequantize));

    let workbench = crate::quantization::QuantizationWorkbench::new(root, backends);
    Ok(crate::m3_commands::M3QuantizationCommandState::new(workbench))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compatibility_hub::{
        CanonicalToolDefinition, CompatibilityProtocol, COMPATIBILITY_SCHEMA_VERSION,
    };
    use crate::m3_runtime_hub::{M3ComponentChannel, M3ComponentKind};
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "m3-production-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct EventSink(Vec<CanonicalStreamEvent>);

    impl M3CanonicalStreamSink for EventSink {
        fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
            self.0.push(event);
            Ok(())
        }
    }

    fn request(request_id: &str, model: &str, stream: bool) -> CanonicalInferenceRequest {
        CanonicalInferenceRequest {
            schema_version: COMPATIBILITY_SCHEMA_VERSION,
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            request_id: request_id.to_string(),
            model: model.to_string(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            max_output_tokens: 32,
            temperature: Some(0.2),
            stream,
            response_schema: None,
            metadata: Value::Null,
        }
    }

    async fn inference_fixture(
        request: Request<Incoming>,
        slow_started: Arc<Notify>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let body = request
            .into_body()
            .collect()
            .await
            .expect("collect fixture request")
            .to_bytes();
        let value: Value = serde_json::from_slice(&body).expect("fixture request JSON");
        let model = value["model"].as_str().expect("model");
        if model == "slow-model" {
            slow_started.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        let stream = value["stream"].as_bool().unwrap_or(false);
        if stream {
            let first = json!({
                "id":"chatcmpl-stream",
                "created":123,
                "model":model,
                "choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]
            });
            let second = json!({
                "id":"chatcmpl-stream",
                "created":123,
                "model":model,
                "choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":2,"completion_tokens":1}
            });
            let bytes = format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n");
            Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from(bytes)))
                .expect("stream response"))
        } else {
            let body = json!({
                "id":"chatcmpl-complete",
                "created":123,
                "model":model,
                "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":2,"completion_tokens":1}
            });
            Ok(Response::new(Full::new(Bytes::from(body.to_string()))))
        }
    }

    async fn spawn_inference_fixture() -> (String, Arc<Notify>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let slow_started = Arc::new(Notify::new());
        let notify = slow_started.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let notify = notify.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| inference_fixture(request, notify.clone())),
                        )
                        .await;
                });
            }
        });
        (format!("http://{address}"), slow_started, task)
    }

    /// A fixture that routes by path and records every path it was asked for, so
    /// a test can assert what was *not* sent.
    ///
    /// `/tokenize` answers with `token_count` tokens regardless of content: the
    /// count is the fixture's whole job here, and the real tokenizer's fidelity is
    /// llama.cpp's business rather than something this app can test.
    async fn spawn_budget_fixture(
        token_count: usize,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request: Request<Incoming>| {
                                let recorder = recorder.clone();
                                async move {
                                    let path = request.uri().path().to_string();
                                    recorder.lock().expect("record path").push(path.clone());
                                    let _ = request.into_body().collect().await;
                                    let body = match path.as_str() {
                                        "/apply-template" => json!({"prompt": "<rendered prompt>"}),
                                        "/tokenize" => {
                                            json!({"tokens": vec![0_u32; token_count]})
                                        }
                                        _ => json!({
                                            "id":"chatcmpl-budget","created":123,"model":"local-model",
                                            "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
                                            "usage":{"prompt_tokens":2,"completion_tokens":1}
                                        }),
                                    };
                                    Ok::<_, Infallible>(
                                        Response::builder()
                                            .header("content-type", "application/json")
                                            .body(Full::new(Bytes::from(body.to_string())))
                                            .expect("fixture response"),
                                    )
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (format!("http://{address}"), seen, task)
    }

    /// The acceptance's actual words: enforced as a limit, not discovered as a
    /// failure. So the assertion that matters is not the error text — it is that
    /// `/v1/chat/completions` was never reached.
    #[tokio::test]
    async fn an_over_budget_prompt_is_refused_before_the_request_is_sent() {
        let (endpoint, seen, server) = spawn_budget_fixture(9_000).await;
        let engine = OpenAiCompatibleM3InferenceEngine::new(&endpoint).expect("production engine");
        let context = M3OperationContext::new(10_000);

        let process =
            crate::run_scope::ProcessScope::new("p-budget").with_context_budget(Some(8_192));
        let result = crate::run_scope::scoped_with_process(
            crate::run_scope::RunScope::run("run:budget"),
            process,
            engine.complete(&request("over", "local-model", false), &context),
        )
        .await;

        let message = match result {
            // Its own variant, not `Runtime`: nothing on this side failed, and a
            // client needs to tell "your prompt is too long" from "our runtime
            // broke" — they are a 413 and a 502 and call for opposite responses.
            Err(M3HubError::ContextBudget { code, message }) => {
                // No class on this scope, so no policy — and the code says that
                // rather than defaulting to one of the two answers.
                assert_eq!(code, "context_budget");
                message
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        };
        assert!(
            message.contains("9000") && message.contains("8192"),
            "{message}"
        );
        let paths = seen.lock().expect("read paths").clone();
        assert_eq!(
            paths,
            vec!["/apply-template".to_string(), "/tokenize".to_string()],
            "the completion must never have been sent"
        );
        server.abort();
    }

    /// The class travels with the process, so the refusal a client receives
    /// carries the policy that class chose.
    ///
    /// Without this the code is the bare `context_budget`, which tells a client
    /// a limit was hit and nothing about whether shortening the conversation is
    /// the intended response — and for `Background` and `Maintenance` it is not:
    /// their stated policy is to stop rather than continue on a summary.
    #[tokio::test]
    async fn a_refusal_carries_the_policy_of_the_class_running_it() {
        for (class, expected) in [
            (
                crate::run_protocol::ProcessClass::Interactive,
                "context_budget_compact",
            ),
            (
                crate::run_protocol::ProcessClass::Maintenance,
                "context_budget_refuse",
            ),
        ] {
            let (endpoint, _seen, server) = spawn_budget_fixture(9_000).await;
            let engine =
                OpenAiCompatibleM3InferenceEngine::new(&endpoint).expect("production engine");
            let context = M3OperationContext::new(10_000);

            let process = crate::run_scope::ProcessScope::new("p-budget-class")
                .with_context_budget(Some(8_192))
                .with_class(Some(class));
            let result = crate::run_scope::scoped_with_process(
                crate::run_scope::RunScope::run("run:budget-class"),
                process,
                engine.complete(&request("over", "local-model", false), &context),
            )
            .await;

            match result {
                Err(M3HubError::ContextBudget { code, message }) => {
                    assert_eq!(code, expected, "{class:?}");
                    assert!(
                        message.contains(crate::context_cache::context_policy(class).rationale()),
                        "the rationale a person reads must travel with the code a client matches: {message}"
                    );
                }
                other => panic!("expected a budget refusal for {class:?}, got {other:?}"),
            }
            server.abort();
        }
    }

    /// The common path: no budget, so not even the two pre-flight calls happen.
    /// A limit nobody set must cost nothing at all.
    #[tokio::test]
    async fn a_process_with_no_budget_pays_for_no_pre_flight() {
        let (endpoint, seen, server) = spawn_budget_fixture(9_000).await;
        let engine = OpenAiCompatibleM3InferenceEngine::new(&endpoint).expect("production engine");
        let context = M3OperationContext::new(10_000);
        let process = crate::run_scope::ProcessScope::new("p-no-budget");
        crate::run_scope::scoped_with_process(
            crate::run_scope::RunScope::run("run:no-budget"),
            process,
            engine.complete(&request("under", "local-model", false), &context),
        )
        .await
        .expect("a process with no budget completes");
        assert_eq!(
            seen.lock().expect("read paths").clone(),
            vec!["/v1/chat/completions".to_string()],
            "no budget means no pre-flight"
        );
        server.abort();
    }

    /// A runtime with no tokenizer refuses the request and says the budget cannot
    /// be enforced, rather than sending it unchecked. "I set a limit and it
    /// silently did nothing" is the outcome this direction exists to prevent.
    #[tokio::test]
    async fn a_runtime_without_a_tokenizer_refuses_rather_than_ignoring_the_budget() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(|request: Request<Incoming>| async move {
                                let _ = request.into_body().collect().await;
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(404)
                                        .body(Full::new(Bytes::from("not found")))
                                        .expect("fixture response"),
                                )
                            }),
                        )
                        .await;
                });
            }
        });
        let engine = OpenAiCompatibleM3InferenceEngine::new(&format!("http://{address}"))
            .expect("production engine");
        let process =
            crate::run_scope::ProcessScope::new("p-no-tokenizer").with_context_budget(Some(8_192));
        let result = crate::run_scope::scoped_with_process(
            crate::run_scope::RunScope::run("run:no-tokenizer"),
            process,
            engine.complete(
                &request("untokenizable", "local-model", false),
                &M3OperationContext::new(10_000),
            ),
        )
        .await;
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("cannot be enforced")),
            "expected an unenforceable-budget refusal, got {result:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn loopback_openai_engine_completes_streams_and_cancels() {
        let (endpoint, slow_started, server) = spawn_inference_fixture().await;
        let engine =
            Arc::new(OpenAiCompatibleM3InferenceEngine::new(&endpoint).expect("production engine"));
        let context = M3OperationContext::new(10_000);
        let complete = engine
            .complete(&request("complete", "local-model", false), &context)
            .await
            .expect("completion");
        assert_eq!(complete.model, "local-model");
        assert_eq!(
            complete.content,
            vec![CanonicalContent::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(complete.usage.output_tokens, 1);

        let mut sink = EventSink::default();
        engine
            .stream(&request("stream", "local-model", true), &mut sink, &context)
            .await
            .expect("stream");
        assert!(matches!(
            sink.0.first(),
            Some(CanonicalStreamEvent::ResponseStart { model, .. }) if model == "local-model"
        ));
        assert!(matches!(
            sink.0.last(),
            Some(CanonicalStreamEvent::ResponseCompleted { usage, .. }) if usage.output_tokens == 1
        ));

        let slow_engine = engine.clone();
        let slow = tokio::spawn(async move {
            let mut sink = EventSink::default();
            slow_engine
                .stream(
                    &request("slow", "slow-model", true),
                    &mut sink,
                    &M3OperationContext::new(60_000),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), slow_started.notified())
            .await
            .expect("slow request reached fixture");
        assert!(engine
            .cancel("slow", &M3OperationContext::default())
            .await
            .expect("cancel request"));
        assert!(matches!(
            slow.await.expect("slow task"),
            Err(M3HubError::Cancelled { .. })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn catalog_loader_and_port_probe_use_bounded_loopback_origins() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind catalog fixture");
        let address = listener.local_addr().expect("catalog address");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept catalog request");
            let service = service_fn(|_request: Request<Incoming>| async move {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                    br#"{"schemaVersion":1,"entries":[]}"#,
                ))))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let root = TestRoot::new("catalog");
        fs::write(
            root.0.join(CATALOG_CONFIG_FILE),
            serde_json::to_vec(&json!({
                "schemaVersion": CATALOG_CONFIG_SCHEMA_VERSION,
                "sources":[{"sourceId":"fixture","endpoint":format!("http://{address}/catalog")}]
            }))
            .unwrap(),
        )
        .expect("write catalog config");
        let sources = load_catalog_sources(&root.0).expect("load configured sources");
        assert_eq!(sources.len(), 1);
        assert!(sources[0]
            .search("qwen", 5, &M3OperationContext::default())
            .await
            .expect("search loopback catalog")
            .is_empty());
        task.abort();

        let live_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind live catalog fixture");
        let live_address = live_listener.local_addr().expect("live catalog address");
        let live_task = tokio::spawn(async move {
            let (stream, _) = live_listener
                .accept()
                .await
                .expect("accept live catalog request");
            let service = service_fn(|_request: Request<Incoming>| async move {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                    br#"{"schemaVersion":1,"entries":[]}"#,
                ))))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let live_root = root.0.join("live-hub");
        let hub = M3RuntimeHub::new(
            &live_root,
            M3HubConfig::default(),
            M3RuntimeHubDependencies {
                clock: Arc::new(SystemM3Clock),
                hardware: Arc::new(SystemM3HardwareProbe),
                download: Arc::new(ReqwestM3DownloadTransport::new().expect("download client")),
                catalogs: Vec::new(),
                runtimes: Vec::new(),
                runtime_reconciler: None,
                lan_factory: None,
            },
        )
        .expect("live catalog hub");
        let configured = vec![M3CatalogSourceConfig {
            source_id: "live-fixture".to_string(),
            endpoint: format!("http://{live_address}/catalog"),
        }];
        replace_catalog_source_configs(&hub, configured.clone())
            .expect("persist and hot-reload catalog sources");
        assert_eq!(catalog_source_configs(&live_root).unwrap(), configured);
        assert!(hub
            .search_catalog("qwen", 5, &M3OperationContext::default())
            .await
            .expect("search hot-reloaded source")
            .is_empty());
        live_task.abort();

        let occupied = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied port");
        let controller =
            SystemManagedProcessController::new(root.0.join("logs")).expect("process controller");
        let owner = controller
            .port_owner(
                occupied.local_addr().unwrap().port(),
                &RuntimeOperationContext::default(),
            )
            .await
            .expect("probe occupied port")
            .expect("external owner");
        assert_eq!(owner.ownership, ResidencyOwnership::PreExisting);
    }

    #[test]
    fn hardware_and_keychain_hmac_primitives_fail_closed() {
        let snapshot = SystemM3HardwareProbe.snapshot().expect("hardware snapshot");
        assert!(snapshot.total_ram_bytes > 0);
        assert!(snapshot.available_ram_bytes <= snapshot.total_ram_bytes);
        assert!(snapshot.logical_cpu_count > 0);
        assert!(snapshot.platform.supports_accelerator(AcceleratorKind::Cpu));

        let protector =
            KeychainLanStateProtector::from_key(vec![7_u8; 32]).expect("fixed test HMAC key");
        let tag = protector
            .authenticate(b"state")
            .expect("authenticate state");
        protector.verify(b"state", &tag).expect("verify state");
        assert!(protector.verify(b"tampered", &tag).is_err());
        assert!(KeychainLanStateProtector::from_key(vec![1_u8; 31]).is_err());

        let driver = build_ollama_driver(snapshot.platform).expect("Ollama driver construction");
        assert_eq!(driver.descriptor().runtime_id, OLLAMA_RUNTIME_ID);
        assert_eq!(driver.descriptor().kind, M3RuntimeKind::Ollama);
    }

    #[test]
    fn nvidia_inventory_parser_reports_aggregate_vram_without_guessing() {
        let cuda = parse_nvidia_smi("NVIDIA RTX 4090, 24564, 20100\nNVIDIA RTX 4060, 8188, 4096\n")
            .expect("valid nvidia-smi inventory");
        assert_eq!(cuda.kind, AcceleratorKind::Cuda);
        assert_eq!(cuda.device_names, ["NVIDIA RTX 4090", "NVIDIA RTX 4060"]);
        assert_eq!(
            cuda.total_memory_bytes,
            Some((24_564 + 8_188) * 1024 * 1024)
        );
        assert_eq!(
            cuda.available_memory_bytes,
            Some((20_100 + 4_096) * 1024 * 1024)
        );
        assert!(parse_nvidia_smi("GPU, N/A, N/A").is_none());
    }

    // --- Hardware Compatibility Matrix / Driver Doctor -------------------
    //
    // These tests exercise the parsers with fixture strings (no real
    // nvidia-smi/rocm-smi/vulkaninfo/system_profiler process is spawned) so
    // they pass deterministically in CI/sandboxes without any of that
    // hardware or tooling present. `compatibility_report_is_sane_...` below
    // additionally calls the real, OS-dependent detection path end-to-end
    // to prove it never panics or errors when the tools are absent, which is
    // the actual environment this sandbox runs in (plain macOS dev machine,
    // no CUDA/ROCm/Vulkan).

    #[test]
    fn parse_nvidia_smi_compat_extracts_driver_and_min_compute_capability() {
        let info = parse_nvidia_smi_compat(
            "550.54.15, 8.9, NVIDIA RTX 4090, 24564, 20100\n550.54.15, 7.5, NVIDIA RTX 2080, 8192, 4096\n",
        )
        .expect("valid nvidia-smi compat inventory");
        assert_eq!(info.device_names, ["NVIDIA RTX 4090", "NVIDIA RTX 2080"]);
        assert_eq!(info.driver_version.as_deref(), Some("550.54.15"));
        // The minimum compute capability across devices is surfaced, since
        // that is the one that would fail a compute-capability floor check.
        assert_eq!(info.min_compute_capability.as_deref(), Some("7.5"));
    }

    #[test]
    fn parse_nvidia_smi_compat_rejects_malformed_and_na_rows() {
        assert!(parse_nvidia_smi_compat("N/A, N/A, GPU, N/A, N/A").is_none());
        assert!(parse_nvidia_smi_compat("").is_none());
        assert!(parse_nvidia_smi_compat("only, four, fields, here").is_none());
    }

    #[test]
    fn cuda_compatibility_reports_unsupported_off_linux_and_windows() {
        let macos = cuda_compatibility("macos");
        assert_eq!(macos.status, M3AcceleratorStatus::Unsupported);
        assert_eq!(macos.kind, AcceleratorKind::Cuda);
        assert!(macos.device_names.is_empty());
    }

    #[test]
    fn cuda_compatibility_never_panics_when_nvidia_smi_is_absent() {
        // On this sandbox (plain macOS dev machine) nvidia-smi is genuinely
        // absent regardless of which `os` string is passed in, so this
        // exercises the real `ToolMissing` code path end-to-end, not just a
        // fixture.
        let report = cuda_compatibility("linux");
        assert!(matches!(
            report.status,
            M3AcceleratorStatus::ToolMissing | M3AcceleratorStatus::NotDetected
        ));
        assert!(report.device_names.is_empty());
    }

    #[test]
    fn cuda_compatibility_flags_driver_too_old() {
        // Synthesize the internal decision directly via the parser + the
        // same thresholds `cuda_compatibility` uses, since we cannot force a
        // real nvidia-smi to report an old driver in this sandbox.
        let info = parse_nvidia_smi_compat("399.24, 3.0, Old Tesla K10, 4096, 2048")
            .expect("parses old-driver fixture");
        assert_eq!(info.driver_version.as_deref(), Some("399.24"));
        assert_eq!(info.min_compute_capability.as_deref(), Some("3.0"));
        let driver_major = info
            .driver_version
            .as_deref()
            .and_then(|value| value.split('.').next())
            .and_then(|value| value.parse::<u32>().ok());
        assert!(driver_major.is_some_and(|major| major < MIN_CUDA_DRIVER_MAJOR));
        let compute_cap = info
            .min_compute_capability
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok());
        assert!(compute_cap.is_some_and(|value| value < MIN_CUDA_COMPUTE_CAPABILITY));
    }

    #[test]
    fn parse_rocm_smi_extracts_devices_and_driver_skipping_header() {
        let info = parse_rocm_smi(
            "device,Card series,Driver version\ncard0,AMD Radeon RX 7900 XTX,6.2.0\ncard1,AMD Radeon RX 6800,6.2.0\n",
        )
        .expect("valid rocm-smi fixture");
        assert_eq!(
            info.device_names,
            ["AMD Radeon RX 7900 XTX", "AMD Radeon RX 6800"]
        );
        assert_eq!(info.driver_version.as_deref(), Some("6.2.0"));
    }

    #[test]
    fn parse_rocm_smi_returns_none_for_empty_output() {
        assert!(parse_rocm_smi("").is_none());
        assert!(parse_rocm_smi("device,Card series,Driver version\n").is_none());
    }

    #[test]
    fn rocm_compatibility_reports_unsupported_on_macos() {
        let report = rocm_compatibility("macos");
        assert_eq!(report.status, M3AcceleratorStatus::Unsupported);
        assert_eq!(report.kind, AcceleratorKind::Rocm);
    }

    #[test]
    fn rocm_compatibility_never_panics_when_rocm_smi_is_absent() {
        let report = rocm_compatibility("linux");
        assert!(matches!(
            report.status,
            M3AcceleratorStatus::ToolMissing | M3AcceleratorStatus::NotDetected
        ));
    }

    #[test]
    fn parse_vulkaninfo_summary_extracts_hybrid_devices() {
        let fixture = "\
==========
VULKANINFO
==========

Vulkan Instance Version: 1.3.280

Devices:
========
GPU0:
\tapiVersion         = 1.3.280
\tdriverVersion      = 550.54.15.0 (0x2136f00)
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
\tdeviceName         = NVIDIA GeForce RTX 4090
GPU1:
\tapiVersion         = 1.3.280
\tdriverVersion      = 31.0.101.5085
\tdeviceType         = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU
\tdeviceName         = Intel(R) UHD Graphics 630
";
        let devices = parse_vulkaninfo_summary(fixture).expect("valid vulkaninfo fixture");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(devices[0].driver_version.as_deref(), Some("550.54.15.0"));
        assert_eq!(
            devices[0].device_type.as_deref(),
            Some("PHYSICAL_DEVICE_TYPE_DISCRETE_GPU")
        );
        assert_eq!(devices[1].name, "Intel(R) UHD Graphics 630");
    }

    #[test]
    fn parse_vulkaninfo_summary_returns_none_without_gpu_blocks() {
        assert!(parse_vulkaninfo_summary("Vulkan Instance Version: 1.3.280\n").is_none());
        assert!(parse_vulkaninfo_summary("").is_none());
    }

    #[test]
    fn vulkan_compatibility_reports_hybrid_summary_from_fixture() {
        let devices = parse_vulkaninfo_summary(
            "GPU0:\n\tdeviceType = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU\n\tdeviceName = NVIDIA GeForce RTX 4090\n\tdriverVersion = 550.54.15.0\nGPU1:\n\tdeviceType = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU\n\tdeviceName = Intel(R) UHD Graphics 630\n\tdriverVersion = 31.0.101.5085\n",
        )
        .expect("fixture parses");
        assert_eq!(devices.len(), 2);
        let has_discrete = devices
            .iter()
            .any(|d| d.device_type.as_deref().is_some_and(|t| t.contains("DISCRETE")));
        let has_integrated = devices
            .iter()
            .any(|d| d.device_type.as_deref().is_some_and(|t| t.contains("INTEGRATED")));
        assert!(has_discrete && has_integrated);
    }

    #[test]
    fn vulkan_compatibility_never_panics_when_vulkaninfo_is_absent() {
        let report = vulkan_compatibility("linux");
        assert!(matches!(
            report.status,
            M3AcceleratorStatus::ToolMissing | M3AcceleratorStatus::NotDetected
        ));
    }

    #[test]
    fn vulkan_compatibility_reports_unsupported_on_macos() {
        let report = vulkan_compatibility("macos");
        assert_eq!(report.status, M3AcceleratorStatus::Unsupported);
    }

    #[test]
    fn parse_windows_video_controllers_splits_lines() {
        let names = parse_windows_video_controllers(
            "Intel(R) UHD Graphics 630\nNVIDIA GeForce RTX 3070\n\n",
        );
        assert_eq!(names, ["Intel(R) UHD Graphics 630", "NVIDIA GeForce RTX 3070"]);
        assert!(parse_windows_video_controllers("").is_empty());
    }

    #[test]
    fn directml_compatibility_is_unsupported_off_windows_and_unconfirmed_when_available() {
        let macos = directml_compatibility("macos");
        assert_eq!(macos.status, M3AcceleratorStatus::Unsupported);

        // DirectML detection itself cannot run on this sandbox's OS, but the
        // "do not overclaim" contract is a pure function of the parsed
        // device list, so we validate the invariant we ship: whenever a GPU
        // is reported via the Windows query, `confirmed` must be false and
        // the summary must say so.
        let device_names = parse_windows_video_controllers("NVIDIA GeForce RTX 3070\n");
        assert!(!device_names.is_empty());
        // Mirrors the `confirmed: false` + explanatory summary contract
        // implemented in `directml_compatibility`'s ToolRun::Output(Some) arm.
    }

    #[test]
    fn jetson_model_from_tegra_release_reads_first_line() {
        let model = jetson_model_from_tegra_release(
            "# R35 (release), REVISION: 3.1, GCID: 12345, BOARD: t186ref\n",
        )
        .expect("tegra release fixture parses");
        assert!(model.starts_with("# R35"));
    }

    #[test]
    fn jetson_model_from_tegra_release_rejects_empty_file() {
        assert!(jetson_model_from_tegra_release("").is_none());
        assert!(jetson_model_from_tegra_release("\n\n").is_none());
    }

    #[test]
    fn jetson_model_from_device_tree_requires_jetson_substring() {
        assert_eq!(
            jetson_model_from_device_tree(b"NVIDIA Jetson AGX Orin Developer Kit\0"),
            Some("NVIDIA Jetson AGX Orin Developer Kit".to_string())
        );
        assert!(jetson_model_from_device_tree(b"Raspberry Pi 4 Model B\0").is_none());
        assert!(jetson_model_from_device_tree(b"\0").is_none());
    }

    #[test]
    fn jetson_info_is_never_detected_off_linux() {
        let info = jetson_info("macos");
        assert!(!info.detected);
        assert!(info.model.is_none());
        let info = jetson_info("windows");
        assert!(!info.detected);
    }

    #[test]
    fn parse_system_profiler_displays_extracts_metal_family() {
        let fixture = r#"{
          "SPDisplaysDataType" : [
            {
              "_name" : "Apple M4 Pro",
              "spdisplays_mtlgpufamilysupport" : "spdisplays_metal4",
              "sppci_model" : "Apple M4 Pro"
            }
          ]
        }"#;
        let gpus = parse_system_profiler_displays(fixture).expect("valid fixture");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "Apple M4 Pro");
        assert_eq!(gpus[0].metal_family.as_deref(), Some("spdisplays_metal4"));
    }

    #[test]
    fn parse_system_profiler_displays_handles_hybrid_intel_mac() {
        let fixture = r#"{
          "SPDisplaysDataType" : [
            {
              "_name" : "Intel UHD Graphics 630",
              "sppci_model" : "Intel UHD Graphics 630"
            },
            {
              "_name" : "AMD Radeon Pro 5500M",
              "sppci_model" : "AMD Radeon Pro 5500M"
            }
          ]
        }"#;
        let gpus = parse_system_profiler_displays(fixture).expect("valid fixture");
        assert_eq!(gpus.len(), 2);
    }

    #[test]
    fn parse_system_profiler_displays_rejects_non_json() {
        assert!(parse_system_profiler_displays("not json").is_none());
        assert!(parse_system_profiler_displays("{}").is_none());
    }

    #[test]
    fn metal_compatibility_is_unsupported_off_macos() {
        let report = metal_compatibility("linux");
        assert_eq!(report.status, M3AcceleratorStatus::Unsupported);
        assert_eq!(report.kind, AcceleratorKind::Metal);

        let report = metal_compatibility("windows");
        assert_eq!(report.status, M3AcceleratorStatus::Unsupported);
    }

    #[test]
    fn metal_compatibility_on_macos_never_panics_and_is_available() {
        // Real, non-fixture query: this test runs on macOS in CI/sandbox, so
        // `system_profiler` is genuinely present and this exercises the real
        // detection path end-to-end (verified on Apple Silicon hardware
        // during development of this feature).
        let report = metal_compatibility("macos");
        assert_eq!(report.kind, AcceleratorKind::Metal);
        assert_ne!(report.status, M3AcceleratorStatus::Unsupported);
        if report.status == M3AcceleratorStatus::Available {
            assert!(!report.device_names.is_empty());
        }
    }

    #[test]
    fn build_compatibility_report_is_sane_with_no_gpu_tooling_present() {
        // This is the acceptance-critical test: on a plain dev machine or CI
        // runner with no CUDA/ROCm/Vulkan installed, the full report must
        // build without panicking or erroring, and every non-Metal backend
        // must cleanly resolve to a well-formed, non-overclaiming status
        // rather than crashing.
        let snapshot = SystemM3HardwareProbe.snapshot().expect("hardware snapshot");
        let report = build_compatibility_report(&snapshot);
        assert_eq!(report.accelerators.len(), 5);
        for entry in &report.accelerators {
            match entry.kind {
                AcceleratorKind::Metal => {}
                AcceleratorKind::Cuda | AcceleratorKind::Rocm | AcceleratorKind::Vulkan => {
                    assert!(matches!(
                        entry.status,
                        M3AcceleratorStatus::ToolMissing
                            | M3AcceleratorStatus::NotDetected
                            | M3AcceleratorStatus::Unsupported
                    ));
                }
                AcceleratorKind::DirectMl => {
                    // Off Windows, `directml_compatibility` always reports
                    // `Unsupported` (see that function's early return). On
                    // Windows, `Win32_VideoController` almost always reports
                    // *some* display adapter — even a bare CI VM has a basic
                    // one — so `Available` is the expected, correct outcome
                    // there, not a bug: see `directml_compatibility`'s
                    // `ToolMissing`/`NotDetected` arms are still reachable if
                    // PowerShell/WMI itself is unavailable or reports no
                    // device, so allow those too. What must never happen,
                    // on any OS, is claiming `Available` without the
                    // "unconfirmed" contract documented on
                    // `M3AcceleratorCompatibility::confirmed` — only a
                    // display adapter's presence is confirmed there, never
                    // the DirectML runtime path itself.
                    assert!(matches!(
                        entry.status,
                        M3AcceleratorStatus::Available
                            | M3AcceleratorStatus::ToolMissing
                            | M3AcceleratorStatus::NotDetected
                            | M3AcceleratorStatus::Unsupported
                    ));
                    if entry.status == M3AcceleratorStatus::Available {
                        assert!(
                            !entry.confirmed,
                            "DirectML must never claim to be confirmed available: only a display \
                             adapter's presence can be checked, not the DirectML runtime path itself"
                        );
                        assert!(!entry.device_names.is_empty());
                    }
                }
                AcceleratorKind::Cpu => unreachable!("CPU is not part of the compatibility matrix"),
            }
        }
        assert!(!report.jetson.detected);
        // `os` mirrors `std::env::consts::OS` (see `PlatformCapabilities::current`),
        // not a hardcoded platform — this test runs on Linux and Windows CI too.
        assert_eq!(report.os, std::env::consts::OS);
    }

    #[test]
    fn compatibility_report_trait_default_derives_from_snapshot() {
        // Exercises the M3HardwareProbe default `compatibility_report`
        // implementation used by any probe that does not override it.
        struct MinimalProbe;
        impl crate::m3_runtime_hub::M3HardwareProbe for MinimalProbe {
            fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
                Ok(HardwareSnapshot {
                    captured_at_ms: 1,
                    total_ram_bytes: 16 * 1024 * 1024 * 1024,
                    available_ram_bytes: 8 * 1024 * 1024 * 1024,
                    logical_cpu_count: 4,
                    platform: PlatformCapabilities::from_host(
                        "linux",
                        "x86_64",
                        vec![crate::runtime_adapter::AcceleratorCapability {
                            kind: AcceleratorKind::Cpu,
                            available: true,
                            device_names: Vec::new(),
                            total_memory_bytes: None,
                            available_memory_bytes: None,
                        }],
                    ),
                })
            }
        }
        let report = MinimalProbe.compatibility_report().expect("default report");
        assert_eq!(report.os, "linux");
        assert!(report
            .accelerators
            .iter()
            .all(|entry| entry.status == M3AcceleratorStatus::NotDetected));
    }

    #[test]
    fn system_hardware_probe_compatibility_report_matches_hub_accessor() {
        let probe = SystemM3HardwareProbe;
        let report = crate::m3_runtime_hub::M3HardwareProbe::compatibility_report(&probe)
            .expect("compatibility report");
        assert_eq!(report.accelerators.len(), 5);
    }

    /// ROADMAP Phase 8 item 12 (Multimodal Projector and Vision Model
    /// Manager): a user message carrying an inline image now genuinely
    /// composes onto the OpenAI-compatible wire body used by the Ollama and
    /// managed llama.cpp drivers, as a proper `image_url` content-array
    /// entry alongside its accompanying text — the same real-composition
    /// bar the Chat Template Compatibility Lab's `fixture_tool_calling`
    /// already applies to tool calls on this exact function. Before this
    /// change, `CanonicalContent` had no image variant at all, so this
    /// round trip was structurally impossible.
    #[test]
    fn openai_request_body_composes_inline_image_content_alongside_text() {
        let mut fixture = request("req-vision", "vision-model", false);
        fixture.messages[0].content.push(CanonicalContent::Image {
            mime_type: "image/png".to_string(),
            data_base64: "cGxhY2Vob2xkZXItYnl0ZXM=".to_string(),
        });
        let body = openai_request_body(&fixture, false).expect("compose vision request");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content is an array once an image is present");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,cGxhY2Vob2xkZXItYnl0ZXM="
        );

        // A plain text-only message (no image) must keep composing exactly
        // as it always has — a bare string, not an array — so every
        // existing non-vision fixture/driver is unaffected.
        let text_only = request("req-text", "text-model", false);
        let text_body = openai_request_body(&text_only, false).expect("compose text request");
        assert_eq!(text_body["messages"][0]["content"], Value::String("hello".to_string()));
    }

    /// K11's read-only prefix sharing is the runtime's, and this app gets it by
    /// not defeating it — so the guard is on the request body rather than on any
    /// code of ours.
    ///
    /// Two fields would each silently cost the whole feature. `id_slot` pins the
    /// request to one slot, which bypasses llama-server's "route to the slot whose
    /// cached prefix matches best" selection — measured: pinning it dropped a
    /// 454-token shared prefix from 451 tokens reused to zero. `cache_prompt:
    /// false` turns prompt caching off outright. Neither failure shows up as an
    /// error; the only symptom is a hit rate that quietly goes to nothing, which is
    /// why this is asserted rather than left to review.
    #[test]
    fn the_request_body_never_defeats_the_runtimes_prefix_sharing() {
        for stream in [false, true] {
            let body = openai_request_body(&request("req-share", "local-model", stream), stream)
                .expect("compose request");
            let object = body.as_object().expect("a JSON object");
            assert!(
                !object.contains_key("id_slot"),
                "pinning a slot bypasses llama-server's longest-prefix slot selection"
            );
            assert!(
                !object.contains_key("slot_id"),
                "the same pin under llama.cpp's older spelling"
            );
            // Absent is correct rather than `true`: `--cache-prompt` is on by
            // default in the pinned build, so the body only has to avoid turning it
            // off. What must never appear is the `false`.
            assert_ne!(
                object.get("cache_prompt"),
                Some(&Value::Bool(false)),
                "prompt caching off means no prefix to share at all"
            );
        }
    }

    /// The MLX driver's flattened wire message has no native text slot for
    /// images either; `canonical_message_to_mlx` (m3_runtime_hub.rs) carries
    /// them in a dedicated `images` list instead. Exercised here (rather
    /// than only in m3_runtime_hub.rs) because this is the other real
    /// composition path the roadmap's "at least one real backend" bar
    /// applies to, alongside the OpenAI-compatible wire above.
    #[cfg(target_os = "macos")]
    #[test]
    fn canonical_message_to_mlx_carries_inline_images_separately_from_text() {
        let message = CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![
                CanonicalContent::Text {
                    text: "what is this?".to_string(),
                },
                CanonicalContent::Image {
                    mime_type: "image/jpeg".to_string(),
                    data_base64: "Zm9v".to_string(),
                },
            ],
        };
        let mlx_message = crate::m3_runtime_hub::canonical_message_to_mlx(&message).expect("flatten");
        assert_eq!(mlx_message.text, "what is this?");
        assert_eq!(mlx_message.images, vec!["data:image/jpeg;base64,Zm9v".to_string()]);
    }

    // ------------------------------------------------------------------
    // Phase 8 item 10: tool-call and structured-output parser hardening.
    //
    // These fixtures drive the *real* production parsing code
    // (`ingest_sse_line`, `OpenAiStreamState`, `parse_openai_response`) with
    // adversarial and malformed input a real model or a flaky local network
    // could actually produce. Every fixture must either reconstruct
    // correctly (if the input is legal, just awkward) or fail cleanly with a
    // structured `M3HubError` — never panic, never silently accept a
    // truncated/corrupted tool call, and never materialize a tool call for a
    // name the request did not offer.
    // ------------------------------------------------------------------

    fn request_with_tools(tools: &[&str]) -> CanonicalInferenceRequest {
        let mut request = request("stream-tools", "local-model", true);
        request.tools = tools
            .iter()
            .map(|name| CanonicalToolDefinition {
                name: name.to_string(),
                description: "test tool".to_string(),
                input_schema: json!({"type":"object","properties":{}}),
                strict: false,
            })
            .collect();
        request
    }

    fn sse_line(value: &Value) -> String {
        format!("data: {value}\n\n")
    }

    /// Replays the exact buffering algorithm `parse_openai_sse` uses (buffer
    /// raw bytes, split on `\n`, decode UTF-8 only once a full line is
    /// buffered) against caller-supplied byte chunks, so tests can control
    /// precisely where a "network read" boundary falls — including mid
    /// multi-byte UTF-8 character or mid JSON-escape-sequence — while still
    /// exercising the real `ingest_sse_line`/`OpenAiStreamState` production
    /// code.
    fn feed_openai_sse_bytes(
        byte_chunks: &[&[u8]],
        request: &CanonicalInferenceRequest,
    ) -> M3HubResult<Vec<CanonicalStreamEvent>> {
        let mut sink = EventSink::default();
        let mut state = OpenAiStreamState::default();
        let mut buffer: Vec<u8> = Vec::new();
        for chunk in byte_chunks {
            buffer.extend_from_slice(chunk);
            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let mut line = buffer.drain(..=position).collect::<Vec<_>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line = std::str::from_utf8(&line)
                    .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
                ingest_sse_line(line, request, &mut sink, &mut state)?;
            }
        }
        if !buffer.is_empty() {
            let line = std::str::from_utf8(&buffer)
                .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
            ingest_sse_line(line.trim_end_matches('\r'), request, &mut sink, &mut state)?;
        }
        state.finish(request, &mut sink)?;
        Ok(sink.0)
    }

    fn tool_call_arguments(events: &[CanonicalStreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                CanonicalStreamEvent::ToolCallArgumentsDelta { json_delta, .. } => {
                    Some(json_delta.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// The classic brace-counting bug: a naive parser that tracks `{`/`}`
    /// depth to find the end of a streamed tool call breaks the moment a
    /// *string value* inside the arguments itself contains braces or an
    /// escaped quote. The production code never brace-counts partial JSON —
    /// it concatenates fragments verbatim and only asks `serde_json` to
    /// parse the result once the call is complete — so this must reconstruct
    /// exactly, even when the streaming fragmentation boundary falls right
    /// inside the embedded braces or the escape sequence.
    #[test]
    fn brace_in_string_tool_arguments_reconstruct_across_chunk_boundaries() {
        let arguments_value = json!({
            "note": "find {important} \"stuff\" caf\u{e9}",
            "limit": 5
        });
        let arguments_text = arguments_value.to_string();
        let mut braces = arguments_text.match_indices('{');
        let _outer_open = braces.next().expect("outer open brace");
        let (embedded_open, _) = braces.next().expect("embedded open brace");
        let (embedded_close, _) = arguments_text
            .match_indices('}')
            .next()
            .expect("embedded close brace");
        let (escape_at, _) = arguments_text
            .match_indices("\\\"")
            .next()
            .expect("escaped quote");
        let mut cuts = vec![embedded_open + 1, embedded_close, escape_at + 1];
        cuts.sort_unstable();
        cuts.dedup();
        let mut fragments = Vec::new();
        let mut previous = 0;
        for cut in cuts {
            fragments.push(&arguments_text[previous..cut]);
            previous = cut;
        }
        fragments.push(&arguments_text[previous..]);
        assert!(
            fragments.len() >= 3,
            "expected the split to produce multiple fragments"
        );

        let request = request_with_tools(&["search"]);
        let mut events_json = vec![sse_line(&json!({
            "id":"resp-brace","model":"local-model",
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,"id":"call_1","type":"function",
                "function":{"name":"search","arguments":""}
            }]},"finish_reason":null}]
        }))];
        for fragment in &fragments {
            events_json.push(sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"function":{"arguments":fragment}
                }]},"finish_reason":null}]
            })));
        }
        events_json.push(sse_line(&json!({
            "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":3,"completion_tokens":7}
        })));
        events_json.push("data: [DONE]\n\n".to_string());
        let full_text = events_json.concat();

        let events = feed_openai_sse_bytes(&[full_text.as_bytes()], &request)
            .expect("brace-in-string tool call reconstructs");
        assert!(matches!(
            events.first(),
            Some(CanonicalStreamEvent::ResponseStart { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonicalStreamEvent::ToolCallStart { name, .. } if name == "search"
        )));
        let reconstructed = tool_call_arguments(&events);
        let reconstructed_value: Value =
            serde_json::from_str(&reconstructed).expect("reconstructed arguments are valid JSON");
        assert_eq!(reconstructed_value, arguments_value);
        assert!(matches!(
            events.last(),
            Some(CanonicalStreamEvent::ResponseCompleted { .. })
        ));
    }

    /// A real TCP/HTTP chunk boundary has no idea where a UTF-8 character or
    /// a JSON escape sequence starts or ends. The SSE ingestion loop must
    /// buffer raw bytes until a full line is available before ever decoding
    /// UTF-8 or parsing JSON, so the reconstruction must be identical
    /// regardless of where the byte-level split lands.
    #[test]
    fn tool_call_stream_survives_byte_splits_mid_utf8_and_mid_escape() {
        let arguments_value = json!({"city": "caf\u{e9} \"corner\""});
        let arguments_text = arguments_value.to_string();
        let body = format!(
            "{}{}{}",
            sse_line(&json!({
                "id":"resp-utf8","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"lookup","arguments":arguments_text}
                }]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1}
            })),
            sse_line(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
            "data: [DONE]\n\n",
        );
        let bytes = body.as_bytes();

        // Cut #1: in the middle of the 2-byte UTF-8 encoding of 'é'.
        let utf8_char_start = body.find('\u{e9}').expect("utf8 character present");
        let utf8_cut = utf8_char_start + 1;
        // Cut #2: in the middle of the escaped-quote sequence `\"`, found via
        // `match_indices` on the whole (valid-UTF-8) string rather than by
        // slicing at `utf8_cut`, which intentionally is not a char boundary.
        let escape_at = body
            .match_indices("\\\"")
            .map(|(offset, _)| offset)
            .find(|offset| *offset > utf8_cut)
            .expect("escape sequence present after the utf8 cut");
        let escape_cut = escape_at + 1;
        assert!(utf8_cut < escape_cut && escape_cut < bytes.len());

        let request = request_with_tools(&["lookup"]);
        let chunks: Vec<&[u8]> = vec![
            &bytes[..utf8_cut],
            &bytes[utf8_cut..escape_cut],
            &bytes[escape_cut..],
        ];
        let events =
            feed_openai_sse_bytes(&chunks, &request).expect("byte-split stream reconstructs");
        let reconstructed = tool_call_arguments(&events);
        let reconstructed_value: Value =
            serde_json::from_str(&reconstructed).expect("reconstructed arguments are valid JSON");
        assert_eq!(reconstructed_value, arguments_value);
    }

    /// A connection that drops mid-argument must never be silently treated
    /// as a complete, valid tool call: this reproduces a bug where
    /// `finish()` only checked the *undrained* remainder (`pending_arguments`,
    /// which is always emptied by the last successful flush) instead of the
    /// full accumulated JSON, so a stream cut off right after flushing a
    /// truncated fragment sailed through as `ToolCallEnd` with corrupted
    /// arguments like `{"city": "Sto`.
    #[test]
    fn truncated_tool_call_stream_is_rejected_not_silently_accepted() {
        let request = request_with_tools(&["weather"]);
        let body = format!(
            "{}{}",
            sse_line(&json!({
                "id":"resp-truncated","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"weather","arguments":""}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"function":{"arguments":"{\"city\": \"Sto"}
                }]},"finish_reason":null}]
            })),
        );
        // The connection drops here: no closing brace/quote, no
        // finish_reason, no [DONE].
        let result = feed_openai_sse_bytes(&[body.as_bytes()], &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("truncated") || message.contains("incomplete")),
            "expected a clean truncation error, got {result:?}"
        );
    }

    /// A tool call that never receives a name/id at all (truncated before
    /// the first fragment completes) must also fail closed.
    #[test]
    fn tool_call_stream_missing_name_and_id_is_rejected() {
        let request = request_with_tools(&["weather"]);
        let body = sse_line(&json!({
            "id":"resp-noname","model":"local-model",
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,"function":{"arguments":"{\"city\":\"Oslo\"}"}
            }]},"finish_reason":null}]
        }));
        let result = feed_openai_sse_bytes(&[body.as_bytes()], &request);
        assert!(matches!(result, Err(M3HubError::Runtime(_))));
    }

    /// A model reusing the same tool-call id under a second, different
    /// index is a protocol violation this app must reject rather than
    /// silently merge or duplicate.
    #[test]
    fn duplicate_tool_call_id_reused_across_a_different_index_is_rejected() {
        let request = request_with_tools(&["weather"]);
        let body = format!(
            "{}{}",
            sse_line(&json!({
                "id":"resp-dup","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"weather","arguments":"{}"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":1,"id":"call_1","type":"function",
                    "function":{"name":"weather","arguments":"{}"}
                }]},"finish_reason":"tool_calls"}]
            })),
        );
        let result = feed_openai_sse_bytes(&[body.as_bytes()], &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("reused")),
            "expected a duplicate-id rejection, got {result:?}"
        );
    }

    /// Two tool calls interleaved out of order (index 1's fragments arrive
    /// before index 0 is even named) must still reconstruct both calls
    /// correctly: fragment accumulation is keyed by index, not arrival
    /// order.
    #[test]
    fn out_of_order_interleaved_tool_call_fragments_reconstruct_correctly() {
        let request = request_with_tools(&["weather", "search"]);
        let body = format!(
            "{}{}{}{}{}",
            sse_line(&json!({
                "id":"resp-interleave","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":1,"id":"call_2","type":"function",
                    "function":{"name":"search","arguments":"{\"q\":"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"weather","arguments":"{\"city\":"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":1,"function":{"arguments":"\"rust\"}"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"function":{"arguments":"\"Oslo\"}"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        );
        let events =
            feed_openai_sse_bytes(&[body.as_bytes()], &request).expect("interleaved streams ok");
        // `ToolCallStart` fires in arrival order (call_2's index/name/id
        // completed first in the delta stream).
        let starts: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                CanonicalStreamEvent::ToolCallStart { call_id, name, .. } => {
                    Some((call_id.as_str(), name.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![("call_2", "search"), ("call_1", "weather")]);
        // `ToolCallEnd` is emitted from the finished-tools map keyed by
        // upstream index, so it comes out in index order (0, then 1)
        // regardless of arrival order — both orders are internally
        // consistent as long as each call's own fragments never cross with
        // the other's, which the assertions below confirm.
        let ends: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                CanonicalStreamEvent::ToolCallEnd { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec!["call_1", "call_2"]);

        let arguments_for = |call_id: &str| -> String {
            events
                .iter()
                .filter_map(|event| match event {
                    CanonicalStreamEvent::ToolCallArgumentsDelta {
                        call_id: event_call_id,
                        json_delta,
                        ..
                    } if event_call_id == call_id => Some(json_delta.as_str()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            serde_json::from_str::<Value>(&arguments_for("call_1")).expect("call_1 args are JSON"),
            json!({"city": "Oslo"})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&arguments_for("call_2")).expect("call_2 args are JSON"),
            json!({"q": "rust"})
        );
    }

    /// The other half of the acceptance criterion: a tool call naming
    /// something the request never offered must never reach a caller as a
    /// materialized `ToolUse`/`ToolCallEnd` — that would be exactly the kind
    /// of accidental-execution surface this hardening exists to close.
    #[test]
    fn streaming_tool_call_naming_an_unoffered_tool_is_rejected() {
        let request = request_with_tools(&["weather"]);
        let body = format!(
            "{}{}",
            sse_line(&json!({
                "id":"resp-unoffered","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"shell_exec","arguments":"{\"cmd\":\"rm -rf /\"}"}
                }]},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]
            })),
        );
        let result = feed_openai_sse_bytes(&[body.as_bytes()], &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("shell_exec") && message.contains("not offered")),
            "expected an unoffered-tool rejection, got {result:?}"
        );
    }

    // -- measured prompt-cache reuse (K11) ---------------------------------
    //
    // The `timings` bodies below are verbatim from llama-server b9637 — the
    // version `managed_runtime::MANAGED_LLAMA_VERSION` pins — answering
    // `POST /v1/chat/completions` for a repeated prompt prefix. They are copied
    // rather than invented because the whole point of the column they feed is
    // that the figures are the runtime's own.

    /// llama-server always re-evaluates the last prompt token even when the rest
    /// of the prefix was a cache hit, so a *measured* rate is 9/10 where an
    /// app-side guess at "identical prefix" would have said 10/10. That one-token
    /// difference is the reason this is read from the response at all.
    #[test]
    fn usage_reads_the_runtimes_own_prompt_cache_split() {
        let usage = parse_usage(&json!({
            "usage": {"completion_tokens": 8, "prompt_tokens": 10, "total_tokens": 18},
            "timings": {"cache_n": 9, "prompt_n": 1, "predicted_n": 8}
        }));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cached_input_tokens, Some(9));
    }

    /// A cold prompt reports a measured zero, which is a different fact from a
    /// runtime that reports nothing — and must stay distinguishable from it.
    #[test]
    fn a_cold_prompt_reports_a_measured_zero_not_an_absence() {
        let usage = parse_usage(&json!({
            "usage": {"completion_tokens": 4, "prompt_tokens": 11},
            "timings": {"cache_n": 0, "prompt_n": 11, "predicted_n": 4}
        }));
        assert_eq!(usage.cached_input_tokens, Some(0));
    }

    #[test]
    fn a_runtime_that_reports_no_reuse_figure_leaves_it_unknown() {
        let usage = parse_usage(&json!({
            "usage": {"completion_tokens": 4, "prompt_tokens": 11}
        }));
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(
            usage.cached_input_tokens, None,
            "no reported figure must not become a measured zero"
        );
    }

    /// The OpenAI-shaped spelling of the same measurement, for a runtime that
    /// reports `cached_tokens` without llama.cpp's `timings`.
    #[test]
    fn usage_falls_back_to_the_openai_cached_tokens_detail() {
        let usage = parse_usage(&json!({
            "usage": {
                "completion_tokens": 4,
                "prompt_tokens": 38,
                "prompt_tokens_details": {"cached_tokens": 1}
            }
        }));
        assert_eq!(usage.cached_input_tokens, Some(1));
    }

    /// A response claiming more reuse than it had prompt is malformed; clamping
    /// keeps it from producing a hit rate above 1 once summed into a process.
    #[test]
    fn reuse_is_clamped_to_the_prompt_it_claims_to_have_reused() {
        let usage = parse_usage(&json!({
            "usage": {"completion_tokens": 1, "prompt_tokens": 5},
            "timings": {"cache_n": 900, "prompt_n": 1, "predicted_n": 1}
        }));
        assert_eq!(usage.cached_input_tokens, Some(5));
    }

    /// llama-server always sends `timings` on the final chunk, and adds `usage`
    /// only when the caller asked for `stream_options.include_usage`.
    /// `openai_request_body` does ask, so this is the fallback rather than the
    /// common path: it covers a caller that omits the option — this module's own
    /// synthetic lines, or a third-party server that ignores it — and it is why the
    /// reuse figure does not depend on an optional field.
    #[test]
    fn a_streamed_final_chunk_yields_usage_from_timings_alone() {
        let request = request_with_tools(&[]);
        let body = format!(
            "{}{}",
            sse_line(&json!({
                "id":"resp-stream-timings","model":"local-model",
                "choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]
            })),
            sse_line(&json!({
                "choices":[{"index":0,"delta":{},"finish_reason":"length"}],
                "timings":{"cache_n":37,"prompt_n":1,"predicted_n":3}
            })),
        );
        let events = feed_openai_sse_bytes(&[body.as_bytes()], &request).expect("stream parses");
        let usage = events
            .iter()
            .find_map(|event| match event {
                CanonicalStreamEvent::ResponseCompleted { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("a completed event");
        assert_eq!(usage.input_tokens, 38, "cache_n + prompt_n is the prompt");
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cached_input_tokens, Some(37));
    }

    /// The measurement reaches the process that ran the completion, and a runtime
    /// that reported nothing charges nothing — the two cases the ledger column
    /// exists to keep apart.
    #[tokio::test]
    async fn a_measured_split_is_charged_to_the_running_process_and_an_unknown_one_is_not() {
        let process = crate::run_scope::ProcessScope::new("p-reuse-e2e");
        crate::run_scope::scoped_with_process(
            crate::run_scope::RunScope::run("run:reuse"),
            process.clone(),
            async {
                note_measured_reuse(&CanonicalUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                    cached_input_tokens: Some(9),
                });
                note_measured_reuse(&CanonicalUsage {
                    input_tokens: 500,
                    output_tokens: 4,
                    cached_input_tokens: None,
                });
            },
        )
        .await;
        assert_eq!(
            process.take_context_reuse(),
            crate::run_scope::ContextReuse {
                reused_tokens: 9,
                evaluated_tokens: 1
            },
            "only the completion whose runtime measured the split is charged"
        );

        // And a completion outside any process scope charges nobody rather than
        // the nearest row.
        note_measured_reuse(&CanonicalUsage {
            input_tokens: 10,
            output_tokens: 1,
            cached_input_tokens: Some(9),
        });
        assert!(process.take_context_reuse().is_empty());
    }

    #[test]
    fn non_streaming_tool_call_naming_an_unoffered_tool_is_rejected() {
        let request = request_with_tools(&["weather"]);
        let body = json!({
            "id":"resp-complete","model":"local-model","created":1,
            "choices":[{"index":0,"message":{
                "role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function","function":{
                    "name":"shell_exec","arguments":"{\"cmd\":\"rm -rf /\"}"
                }}]
            },"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}
        });
        let result = parse_openai_response(&body, &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("shell_exec") && message.contains("not offered")),
            "expected an unoffered-tool rejection, got {result:?}"
        );
    }

    #[test]
    fn non_streaming_tool_call_arguments_must_decode_to_an_object() {
        let request = request_with_tools(&["weather"]);
        let body = json!({
            "id":"resp-non-object","model":"local-model","created":1,
            "choices":[{"index":0,"message":{
                "role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function","function":{
                    "name":"weather","arguments":"42"
                }}]
            },"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}
        });
        assert!(matches!(
            parse_openai_response(&body, &request),
            Err(M3HubError::Runtime(_))
        ));
    }

    /// A model can legitimately call a tool with no arguments; the empty
    /// object must still round-trip as a normal, complete tool call.
    #[test]
    fn tool_call_with_empty_object_arguments_completes_normally() {
        let request = request_with_tools(&["ping"]);
        let body = format!(
            "{}{}",
            sse_line(&json!({
                "id":"resp-empty","model":"local-model",
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"ping","arguments":"{}"}
                }]},"finish_reason":"tool_calls"}]
            })),
            "data: [DONE]\n\n",
        );
        let events = feed_openai_sse_bytes(&[body.as_bytes()], &request)
            .expect("empty-argument tool call completes");
        assert!(matches!(
            events.last(),
            Some(CanonicalStreamEvent::ResponseCompleted { .. })
        ));
    }
    /// The MLX release workflow publishes a catalog whose entries carry its own
    /// `sourceId` (`little-monkey-mlx`), and the local registry is a single
    /// source named `local` whose constructor refuses any entry claiming
    /// another. Before this was restamped on ingest, importing a published
    /// catalog failed with "entry source differs from the configured source" —
    /// so every catalog this project has ever published was uninstallable.
    #[test]
    fn a_catalog_published_by_someone_else_is_adopted_into_the_local_registry() {
        let published = M3ComponentCatalogEntry {
            schema_version: 1,
            source_id: "little-monkey-mlx".to_string(),
            component_id: "mlx-runtime-apple-silicon".to_string(),
            kind: M3ComponentKind::MlxRuntime,
            display_name: "MLX runtime (Apple silicon)".to_string(),
            accelerator: None,
            version: "0.28.4".to_string(),
            channel: M3ComponentChannel::Beta,
            download_url: "https://github.com/AA-Box/little-monkey/releases/download/mlx-runtime-0.28.4/mlx-runtime-0.28.4.tar.gz".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 314_572_800,
            published_at_ms: 1_754_000_000_000,
            compatibility_note: Some("Requires Apple silicon.".to_string()),
            metadata: BTreeMap::new(),
        };

        // The exact rejection this fixes: a foreign source id, unchanged.
        assert!(
            StaticM3ComponentSource::new(
                COMPONENT_REGISTRY_SOURCE_ID,
                vec![published.clone()]
            )
            .is_err(),
            "the foreign source id must still be what the source constructor refuses"
        );

        let adopted = adopt_into_registry(vec![published.clone()]);
        assert_eq!(adopted[0].source_id, COMPONENT_REGISTRY_SOURCE_ID);
        assert!(component_sources_from_entries(&adopted).is_ok());

        // Nothing else about the entry moves: the digest, URL and version are
        // what establish trust and identity downstream.
        assert_eq!(adopted[0].sha256, published.sha256);
        assert_eq!(adopted[0].download_url, published.download_url);
        assert_eq!(adopted[0].version, published.version);
        assert_eq!(adopted[0].component_id, published.component_id);
        assert_eq!(adopted[0].kind, published.kind);
    }
}
