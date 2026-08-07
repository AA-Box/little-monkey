use little_monkey_lib::compatibility_hub::{
    ApiBackend, ApiScope, CompatibilityProtocol, LanEntropySource, LanServerPolicy,
    LanStateProtector, PairedToken, PairingRequest, RateLimitPolicy, SecurityAuditKind, TlsPolicy,
};
use little_monkey_lib::m3_runtime_hub::*;
use little_monkey_lib::model_retirement::STALE_LOCAL_MODEL_THRESHOLD_MS;
use little_monkey_lib::runtime_adapter::{
    AdvancedSettingCapability, EndpointOrigin, EndpointPolicy, HardwareSnapshot, ModelCapabilities,
    PlatformCapabilities, ResidencyOwnership, RunningModel, RuntimeDescriptor, RuntimeInventory,
    RuntimeLifecycleState, RuntimeLogTail, RuntimeModel, RuntimeStatus, SettingValue,
    SettingValueSchema, RUNTIME_ADAPTER_SCHEMA_VERSION,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "m3-hub-{label}-{}-{}",
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

struct FixedClock(AtomicU64);

impl FixedClock {
    fn new(start: u64) -> Self {
        Self(AtomicU64::new(start))
    }
}

impl M3Clock for FixedClock {
    fn now_ms(&self) -> M3HubResult<u64> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// A clock whose value is set explicitly by the test rather than
/// auto-incrementing like [`FixedClock`] — needed to simulate real time
/// passing (e.g. "this model was installed, then 200 days went by") without
/// looping `now_ms()` billions of times.
struct ControllableClock(AtomicU64);

impl ControllableClock {
    fn new(start: u64) -> Self {
        Self(AtomicU64::new(start))
    }

    fn set(&self, value: u64) {
        self.0.store(value, Ordering::SeqCst);
    }
}

impl M3Clock for ControllableClock {
    fn now_ms(&self) -> M3HubResult<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct FixedHardware(HardwareSnapshot);

impl M3HardwareProbe for FixedHardware {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        Ok(self.0.clone())
    }
}

fn hardware() -> HardwareSnapshot {
    HardwareSnapshot {
        captured_at_ms: 1_000,
        total_ram_bytes: 16 * 1024 * 1024 * 1024,
        available_ram_bytes: 12 * 1024 * 1024 * 1024,
        logical_cpu_count: 8,
        platform: PlatformCapabilities::from_host("linux", "x86_64", Vec::new()),
    }
}

struct StaticCatalog {
    source_id: String,
    entries: Vec<M3CatalogModel>,
}

impl M3CatalogSource for StaticCatalog {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<M3CatalogModel>> {
        Box::pin(async move {
            Ok(self
                .entries
                .iter()
                .filter(|entry| {
                    entry
                        .display_name
                        .to_ascii_lowercase()
                        .contains(&query.to_ascii_lowercase())
                })
                .take(limit)
                .cloned()
                .collect())
        })
    }
}

struct DownloadState {
    bytes: Vec<u8>,
    etag: String,
    fail_once_at: Option<u64>,
    corrupt_at: Option<u64>,
    offsets: Vec<u64>,
}

struct MutableDownload {
    state: Mutex<DownloadState>,
}

impl MutableDownload {
    fn new(bytes: Vec<u8>, etag: &str) -> Self {
        Self {
            state: Mutex::new(DownloadState {
                bytes,
                etag: etag.to_string(),
                fail_once_at: None,
                corrupt_at: None,
                offsets: Vec::new(),
            }),
        }
    }

    fn set_payload(&self, bytes: Vec<u8>, etag: &str) {
        let mut state = self.state.lock().expect("download state");
        state.bytes = bytes;
        state.etag = etag.to_string();
        state.fail_once_at = None;
    }

    fn fail_once_at(&self, offset: u64) {
        self.state.lock().expect("download state").fail_once_at = Some(offset);
    }

    /// Returns a structurally valid chunk (correct offset/length/etag) at
    /// `offset` but with a flipped bit, simulating silent transport-level
    /// corruption that framing checks alone cannot catch.
    fn corrupt_chunk_at(&self, offset: u64) {
        self.state.lock().expect("download state").corrupt_at = Some(offset);
    }

    fn offsets(&self) -> Vec<u64> {
        self.state.lock().expect("download state").offsets.clone()
    }
}

impl M3DownloadTransport for MutableDownload {
    fn probe<'a>(
        &'a self,
        _url: &'a str,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadProbe> {
        Box::pin(async move {
            let state = self.state.lock().map_err(|_| M3HubError::LockPoisoned)?;
            Ok(M3DownloadProbe {
                total_bytes: state.bytes.len() as u64,
                etag: Some(state.etag.clone()),
                accepts_ranges: true,
            })
        })
    }

    fn read_range<'a>(
        &'a self,
        _url: &'a str,
        offset: u64,
        max_bytes: usize,
        _expected_etag: Option<&'a str>,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadChunk> {
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| M3HubError::LockPoisoned)?;
            state.offsets.push(offset);
            if state.fail_once_at == Some(offset) {
                state.fail_once_at = None;
                return Err(M3HubError::Transport(
                    "injected one-shot range failure".to_string(),
                ));
            }
            let start = usize::try_from(offset)
                .map_err(|_| M3HubError::Transport("offset overflow".to_string()))?;
            if start >= state.bytes.len() {
                return Err(M3HubError::Transport("range starts past EOF".to_string()));
            }
            let end = start.saturating_add(max_bytes).min(state.bytes.len());
            let mut bytes = state.bytes[start..end].to_vec();
            if state.corrupt_at == Some(offset) {
                state.corrupt_at = None;
                if let Some(first) = bytes.first_mut() {
                    *first ^= 0xFF;
                }
            }
            Ok(M3DownloadChunk {
                offset,
                total_bytes: state.bytes.len() as u64,
                etag: Some(state.etag.clone()),
                bytes,
            })
        })
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn payload(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn catalog_model(bytes: &[u8], revision: &str) -> M3CatalogModel {
    M3CatalogModel {
        schema_version: M3_CATALOG_SCHEMA_VERSION,
        source_id: "test-catalog".to_string(),
        model_id: "local-model".to_string(),
        display_name: "Local Model".to_string(),
        runtime: M3RuntimeKind::LlamaCpp,
        variant_id: "q4_k_m".to_string(),
        revision: revision.to_string(),
        quantization: Some("Q4_K_M".to_string()),
        download_url: "https://models.example.test/local-model.gguf".to_string(),
        sha256: sha256(bytes),
        size_bytes: bytes.len() as u64,
        estimated_ram_bytes: 1024 * 1024 * 1024,
        estimated_vram_bytes: 0,
        supported_os: BTreeSet::from(["linux".to_string()]),
        supported_arch: BTreeSet::from(["x86_64".to_string()]),
        required_accelerator: Some("cpu".to_string()),
        capabilities: M3ModelCapabilities {
            chat: true,
            embeddings: false,
            tool_calling: true,
            vision: false,
            structured_output: true,
        },
        license: M3ModelLicense {
            name: "Apache-2.0".to_string(),
            spdx_id: Some("Apache-2.0".to_string()),
            source_url: "https://models.example.test/LICENSE".to_string(),
            revision: revision.to_string(),
            retrieved_at_ms: 1_000,
            raw_declaration: "Apache License 2.0 test declaration".to_string(),
        },
        metadata: BTreeMap::from([("publisher".to_string(), "test".to_string())]),
        template: None,
        projector: None,
        catalog_retrieved_at_ms: None,
    }
}

fn test_config() -> M3HubConfig {
    M3HubConfig {
        schema_version: M3_HUB_SCHEMA_VERSION,
        storage_quota_bytes: 16 * 1024 * 1024,
        storage_reserve_bytes: 1024 * 1024,
        download_chunk_bytes: 64 * 1024,
        operation_timeout_ms: 10_000,
        max_catalog_results: 100,
    }
}

fn make_hub(
    root: &Path,
    download: Arc<dyn M3DownloadTransport>,
    catalogs: Vec<Arc<dyn M3CatalogSource>>,
    runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
    runtime_reconciler: Option<Arc<dyn M3RuntimeReconciler>>,
    lan_factory: Option<Arc<dyn M3LanAccessFactory>>,
) -> M3RuntimeHub {
    make_hub_with_hardware(
        root,
        download,
        catalogs,
        runtimes,
        runtime_reconciler,
        lan_factory,
        hardware(),
    )
}

/// Same as [`make_hub`] but with an explicit hardware snapshot — used to
/// exercise the Sampler/Batching/Speculative Decoding Controls gating
/// (ROADMAP Phase 8 item 17), which depends on whether the Hardware
/// Compatibility report shows a real GPU backend.
#[allow(clippy::too_many_arguments)]
fn make_hub_with_hardware(
    root: &Path,
    download: Arc<dyn M3DownloadTransport>,
    catalogs: Vec<Arc<dyn M3CatalogSource>>,
    runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
    runtime_reconciler: Option<Arc<dyn M3RuntimeReconciler>>,
    lan_factory: Option<Arc<dyn M3LanAccessFactory>>,
    snapshot: HardwareSnapshot,
) -> M3RuntimeHub {
    M3RuntimeHub::new(
        root,
        test_config(),
        M3RuntimeHubDependencies {
            clock: Arc::new(FixedClock::new(10_000)),
            hardware: Arc::new(FixedHardware(snapshot)),
            download,
            catalogs,
            runtimes,
            runtime_reconciler,
            lan_factory,
        },
    )
    .expect("M3 hub")
}

/// A hardware snapshot reporting an available CUDA GPU — the counterpart to
/// [`hardware`]'s CPU-only default, used to prove the flash-attention/
/// mixed-precision gates actually flip to "supported" on real GPU hardware
/// rather than always reporting unsupported.
fn hardware_with_cuda() -> HardwareSnapshot {
    HardwareSnapshot {
        captured_at_ms: 1_000,
        total_ram_bytes: 16 * 1024 * 1024 * 1024,
        available_ram_bytes: 12 * 1024 * 1024 * 1024,
        logical_cpu_count: 8,
        platform: PlatformCapabilities::from_host(
            "linux",
            "x86_64",
            vec![little_monkey_lib::runtime_adapter::AcceleratorCapability {
                kind: little_monkey_lib::runtime_adapter::AcceleratorKind::Cuda,
                available: true,
                device_names: vec!["Test GPU".to_string()],
                total_memory_bytes: Some(24 * 1024 * 1024 * 1024),
                available_memory_bytes: Some(20 * 1024 * 1024 * 1024),
            }],
        ),
    }
}

#[tokio::test]
async fn download_resumes_verifies_license_checksum_updates_and_deletes_atomically() {
    let directory = TestDirectory::new("download");
    let first_bytes = payload(160_000, 7);
    let download = Arc::new(MutableDownload::new(first_bytes.clone(), "etag-v1"));
    download.fail_once_at(64 * 1024);
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let model = catalog_model(&first_bytes, "rev-1");
    let context = M3OperationContext::new(10_000);

    let wrong_license = M3DownloadRequest {
        model: model.clone(),
        accepted_license_sha256: "0".repeat(64),
    };
    assert!(matches!(
        hub.download_model(&wrong_license, &context).await,
        Err(M3HubError::Forbidden(_))
    ));
    assert!(download.offsets().is_empty());

    let request = M3DownloadRequest {
        accepted_license_sha256: model.license.declaration_sha256(),
        model: model.clone(),
    };
    assert!(matches!(
        hub.download_model(&request, &context).await,
        Err(M3HubError::Transport(_))
    ));
    assert_eq!(download.offsets(), vec![0, 64 * 1024]);
    let partial = fs::read_dir(directory.0.join("downloads"))
        .expect("downloads")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".partial"))
        .expect("partial file");
    assert_eq!(
        partial.metadata().expect("partial metadata").len(),
        64 * 1024
    );

    let installed = hub
        .download_model(&request, &context)
        .await
        .expect("resume verified download");
    assert_eq!(installed.versions.len(), 1);
    assert_eq!(
        fs::read(&installed.versions[0].artifact_path).unwrap(),
        first_bytes
    );
    assert!(download.offsets()[2..].starts_with(&[64 * 1024]));
    assert!(hub.storage_status().expect("storage").used_bytes > model.size_bytes);
    let first_version_key = installed.active_version_key.clone();

    let second_bytes = payload(175_000, 19);
    download.set_payload(second_bytes.clone(), "etag-v2");
    let updated_model = catalog_model(&second_bytes, "rev-2");
    let updated = hub
        .update_model(
            &model.asset_id(),
            &M3DownloadRequest {
                accepted_license_sha256: updated_model.license.declaration_sha256(),
                model: updated_model,
            },
            &context,
        )
        .await
        .expect("verified update");
    assert_eq!(updated.versions.len(), 2);
    let second_artifact_path = updated
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active update")
        .artifact_path
        .clone();
    assert_eq!(
        fs::read(
            &updated
                .versions
                .iter()
                .find(|version| version.active)
                .expect("active update")
                .artifact_path
        )
        .unwrap(),
        second_bytes
    );

    let rolled_back = hub
        .activate_model_version(
            &M3ActivateModelVersionRequest {
                asset_id: model.asset_id(),
                version_key: first_version_key.clone(),
            },
            &context,
        )
        .await
        .expect("activate verified prior version");
    assert_eq!(rolled_back.active_version_key, first_version_key);
    assert_eq!(
        rolled_back
            .versions
            .iter()
            .find(|version| version.active)
            .unwrap()
            .revision,
        "rev-1"
    );
    assert!(matches!(
        hub.prune_model_versions(
            &M3PruneModelVersionsRequest {
                asset_id: model.asset_id(),
                confirmation: "yes".to_string(),
            },
            &context,
        )
        .await,
        Err(M3HubError::Forbidden(_))
    ));
    let pruned = hub
        .prune_model_versions(
            &M3PruneModelVersionsRequest {
                asset_id: model.asset_id(),
                confirmation: format!("PRUNE {}", model.asset_id()),
            },
            &context,
        )
        .await
        .expect("prune inactive version");
    assert_eq!(pruned.versions.len(), 1);
    assert_eq!(pruned.active_version_key, first_version_key);
    assert!(!second_artifact_path.exists());

    let interrupted = directory.0.join("downloads").join("fixture.partial");
    fs::write(&interrupted, b"partial").expect("interrupted download");
    let trash = directory.0.join("models").join(".trash-fixture");
    fs::create_dir(&trash).expect("trash directory");
    fs::write(trash.join("owned"), b"trash").expect("trash payload");
    let asset_root = pruned.versions[0]
        .artifact_path
        .parent()
        .and_then(Path::parent)
        .expect("asset root");
    let staging = asset_root.join(".staging-fixture");
    fs::create_dir(&staging).expect("staging directory");
    fs::write(staging.join("owned"), b"stage").expect("staging payload");
    let cleanup = hub
        .cleanup_orphans("CLEAN ORPHANS", &context)
        .await
        .expect("bounded orphan cleanup");
    assert_eq!(cleanup.removed_paths, 3);
    assert!(cleanup.reclaimed_bytes >= 17);
    assert!(!interrupted.exists());
    assert!(!trash.exists());
    assert!(!staging.exists());

    assert!(matches!(
        hub.delete_model(
            &M3DeleteModelRequest {
                asset_id: model.asset_id(),
                confirmation: "yes".to_string(),
            },
            &context
        )
        .await,
        Err(M3HubError::Forbidden(_))
    ));
    assert!(hub
        .delete_model(
            &M3DeleteModelRequest {
                asset_id: model.asset_id(),
                confirmation: format!("DELETE {}", model.asset_id()),
            },
            &context,
        )
        .await
        .expect("delete"));
    assert!(hub.list_installed_models().unwrap().is_empty());

    let corrupt_bytes = payload(96_000, 31);
    download.set_payload(corrupt_bytes.clone(), "etag-corrupt");
    let mut corrupt_model = catalog_model(&corrupt_bytes, "rev-corrupt");
    corrupt_model.sha256 = "f".repeat(64);
    assert!(matches!(
        hub.download_model(
            &M3DownloadRequest {
                accepted_license_sha256: corrupt_model.license.declaration_sha256(),
                model: corrupt_model,
            },
            &context,
        )
        .await,
        Err(M3HubError::Integrity { .. })
    ));
    assert!(hub.list_installed_models().unwrap().is_empty());
}

#[tokio::test]
async fn catalog_hardware_fit_is_bounded_deduplicated_and_explicit_about_mlx() {
    let directory = TestDirectory::new("catalog");
    let bytes = payload(80_000, 2);
    let cpu = catalog_model(&bytes, "rev-cpu");
    let mut mlx = cpu.clone();
    mlx.runtime = M3RuntimeKind::Mlx;
    mlx.variant_id = "mlx-4bit".to_string();
    mlx.revision = "rev-mlx".to_string();
    mlx.required_accelerator = Some("metal".to_string());
    let download = Arc::new(MutableDownload::new(bytes, "etag"));
    let hub = make_hub(
        &directory.0,
        download,
        vec![Arc::new(StaticCatalog {
            source_id: "test-catalog".to_string(),
            entries: vec![cpu, mlx],
        })],
        Vec::new(),
        None,
        None,
    );
    let matches = hub
        .search_catalog("local", 10, &M3OperationContext::default())
        .await
        .expect("catalog search");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].fit.rating, M3HardwareFitRating::Recommended);
    assert_eq!(matches[1].fit.rating, M3HardwareFitRating::Incompatible);
    assert!(matches[1]
        .fit
        .reasons
        .iter()
        .any(|reason| reason.contains("Apple Silicon")));
    assert!(hub
        .search_catalog("", 10, &M3OperationContext::default())
        .await
        .is_err());
    let manifest = hub.conformance_manifest();
    assert!(!manifest.workspace_tool_routes_exposed);
    assert_eq!(manifest.endpoints.len(), 3);
}

#[derive(Default)]
struct MockRuntimeState {
    models: Mutex<BTreeMap<String, (PathBuf, u64)>>,
    loaded: Mutex<BTreeSet<String>>,
    cancelled: Mutex<Vec<String>>,
    hold_stream: AtomicBool,
    stream_started: Notify,
    stream_release: Notify,
    hold_completion: AtomicBool,
    completion_started: Notify,
    completion_release: Notify,
}

struct MockRuntimeDriver {
    state: Arc<MockRuntimeState>,
}

impl MockRuntimeDriver {
    fn new(state: Arc<MockRuntimeState>) -> Self {
        Self { state }
    }

    fn descriptor_value() -> M3RuntimeDescriptor {
        M3RuntimeDescriptor {
            runtime_id: "managed-llama".to_string(),
            kind: M3RuntimeKind::LlamaCpp,
            label: "Managed llama.cpp".to_string(),
            managed: true,
            api_backend: ApiBackend::ManagedLocal,
        }
    }

    fn native_descriptor() -> RuntimeDescriptor {
        RuntimeDescriptor {
            schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
            runtime_id: "managed-llama".to_string(),
            kind: little_monkey_lib::runtime_adapter::RuntimeKind::LlamaCpp,
            label: "Managed llama.cpp".to_string(),
            endpoint: EndpointOrigin::parse("http://127.0.0.1:8080", EndpointPolicy::LoopbackOnly)
                .expect("endpoint"),
            managed: true,
        }
    }

    fn setting_capabilities() -> Vec<AdvancedSettingCapability> {
        vec![
            AdvancedSettingCapability {
                key: "threads".to_string(),
                label: "Threads".to_string(),
                description: "Worker thread count".to_string(),
                schema: SettingValueSchema::Integer {
                    min: 1,
                    max: 64,
                    step: 1,
                },
                default_value: SettingValue::Integer { value: 4 },
                restart_required: true,
                supported: true,
                unsupported_reason: None,
            },
            // Mirrors `runtime_adapter.rs`'s real llama.cpp capability
            // declarations closely enough to exercise the Runtime Hub's
            // gating layer (ROADMAP Phase 8 item 17) against this mock
            // driver: `M3RuntimeHub::set_runtime_config`/`load_model` gate
            // these by key regardless of which concrete driver declared
            // them.
            AdvancedSettingCapability {
                key: "flash_attention".to_string(),
                label: "Flash attention".to_string(),
                description: "Flash attention behavior".to_string(),
                schema: SettingValueSchema::Choice {
                    options: vec!["auto".to_string(), "on".to_string(), "off".to_string()],
                },
                default_value: SettingValue::Choice {
                    value: "auto".to_string(),
                },
                restart_required: true,
                supported: true,
                unsupported_reason: None,
            },
            AdvancedSettingCapability {
                key: "mixed_precision".to_string(),
                label: "Mixed precision (KV cache)".to_string(),
                description: "KV cache quantization".to_string(),
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
                description: "Draft model id for speculative decoding".to_string(),
                schema: SettingValueSchema::Text { max_bytes: 256 },
                default_value: SettingValue::Text {
                    value: String::new(),
                },
                restart_required: true,
                supported: false,
                unsupported_reason: Some(
                    "Select a model to check for a compatible installed draft model.".to_string(),
                ),
            },
        ]
    }

    fn running_models(&self) -> M3HubResult<Vec<RunningModel>> {
        let loaded = self
            .state
            .loaded
            .lock()
            .map_err(|_| M3HubError::LockPoisoned)?;
        let models = self
            .state
            .models
            .lock()
            .map_err(|_| M3HubError::LockPoisoned)?;
        Ok(loaded
            .iter()
            .map(|model_id| RunningModel {
                runtime_id: "managed-llama".to_string(),
                model_id: model_id.clone(),
                size_bytes: models.get(model_id).map_or(1, |model| model.1),
                memory_bytes: models.get(model_id).map_or(1, |model| model.1),
                vram_bytes: 0,
                digest: None,
                expires_at: None,
                ownership: ResidencyOwnership::AppManaged,
            })
            .collect())
    }

    fn status_value(&self) -> M3HubResult<M3RuntimeStatusView> {
        let running_models = self.running_models()?;
        Ok(M3RuntimeStatusView::Adapter {
            status: RuntimeStatus {
                runtime: Self::native_descriptor(),
                state: RuntimeLifecycleState::Ready,
                version: Some("test-1".to_string()),
                process: None,
                message: None,
                checked_at_ms: 20_000,
            },
            running_models,
        })
    }
}

impl M3RuntimeDriver for MockRuntimeDriver {
    fn descriptor(&self) -> M3RuntimeDescriptor {
        Self::descriptor_value()
    }

    fn capabilities(&self) -> M3RuntimeCapabilityView {
        M3RuntimeCapabilityView {
            descriptor: self.descriptor(),
            can_load: true,
            can_unload: true,
            can_logs: true,
            can_metrics: true,
            can_infer: true,
            can_embed: false,
            settings: Self::setting_capabilities(),
        }
    }

    fn validate_config(&self, values: &BTreeMap<String, SettingValue>) -> M3HubResult<()> {
        little_monkey_lib::runtime_adapter::validate_setting_values(
            "managed-llama",
            &Self::setting_capabilities(),
            values,
            128 * 1024,
        )
        .map_err(|error| M3HubError::Runtime(error.to_string()))
    }

    fn status<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeStatusView> {
        Box::pin(async move { self.status_value() })
    }

    fn inventory<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            let models = self
                .state
                .models
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?
                .iter()
                .map(|(model_id, (path, size))| RuntimeModel {
                    model_id: model_id.clone(),
                    display_name: model_id.clone(),
                    size_bytes: *size,
                    local_path: Some(path.clone()),
                    digest: None,
                    modified_at: None,
                    capabilities: ModelCapabilities {
                        chat: true,
                        embeddings: false,
                        tool_calling: true,
                        vision: false,
                    },
                    metadata: BTreeMap::new(),
                })
                .collect();
            Ok(RuntimeInventory {
                schema_version: RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id: "managed-llama".to_string(),
                models,
                captured_at_ms: 20_000,
            })
        })
    }

    fn load<'a>(
        &'a self,
        model: &'a M3ResolvedModel,
        _settings: &'a BTreeMap<String, SettingValue>,
        _keep_alive: Option<little_monkey_lib::runtime_adapter::KeepAlive>,
        _replace_existing: bool,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let models = self
                .state
                .models
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?;
            let Some((path, _)) = models.get(&model.model_id) else {
                return Err(M3HubError::Conflict(
                    "runtime inventory was not reconciled".to_string(),
                ));
            };
            if path != &model.artifact_path || !path.is_file() {
                return Err(M3HubError::Conflict(
                    "runtime model path differs from managed storage".to_string(),
                ));
            }
            drop(models);
            self.state
                .loaded
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?
                .insert(model.model_id.clone());
            Ok(())
        })
    }

    fn unload<'a>(
        &'a self,
        model_id: &'a str,
        _force_exact_owner: bool,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .loaded
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?
                .remove(model_id);
            Ok(())
        })
    }

    fn logs<'a>(
        &'a self,
        max_bytes: usize,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            let text = "ready\ngenerated\n";
            Ok(RuntimeLogTail {
                text: text[..text.len().min(max_bytes)].to_string(),
                truncated: max_bytes < text.len(),
            })
        })
    }

    fn metrics<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeMetricsView> {
        Box::pin(async move {
            match self.status_value()? {
                M3RuntimeStatusView::Adapter {
                    status,
                    running_models,
                } => Ok(M3RuntimeMetricsView::Adapter {
                    status,
                    running_models,
                }),
                // `Adapter` is the whole enum unless the macOS-only MLX variant
                // is compiled in, and a catch-all over one variant is dead code.
                #[cfg(target_os = "macos")]
                _ => unreachable!(),
            }
        })
    }

    fn complete<'a>(
        &'a self,
        request: &'a little_monkey_lib::compatibility_hub::CanonicalInferenceRequest,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, little_monkey_lib::compatibility_hub::CanonicalInferenceResponse> {
        Box::pin(async move {
            if self.state.hold_completion.load(Ordering::SeqCst) {
                self.state.completion_started.notify_one();
                self.state.completion_release.notified().await;
            }
            Ok(
                little_monkey_lib::compatibility_hub::CanonicalInferenceResponse {
                    response_id: format!("response-{}", request.request_id),
                    model: request.model.clone(),
                    content: vec![
                        little_monkey_lib::compatibility_hub::CanonicalContent::Text {
                            text: "functional response".to_string(),
                        },
                    ],
                    finish_reason: "stop".to_string(),
                    usage: little_monkey_lib::compatibility_hub::CanonicalUsage {
                        input_tokens: 4,
                        output_tokens: 2,
                    },
                    created_at_seconds: 20,
                },
            )
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a little_monkey_lib::compatibility_hub::CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            if self.state.hold_stream.load(Ordering::SeqCst) {
                self.state.stream_started.notify_one();
                self.state.stream_release.notified().await;
            }
            use little_monkey_lib::compatibility_hub::{CanonicalStreamEvent, CanonicalUsage};
            let response_id = format!("response-{}", request.request_id);
            sink.emit(CanonicalStreamEvent::ResponseStart {
                response_id: response_id.clone(),
                model: request.model.clone(),
                created_at_seconds: 20,
            })
            .map_err(M3HubError::Runtime)?;
            sink.emit(CanonicalStreamEvent::TextStart { index: 0 })
                .and_then(|_| {
                    sink.emit(CanonicalStreamEvent::TextDelta {
                        index: 0,
                        text: "streamed".to_string(),
                    })
                })
                .and_then(|_| sink.emit(CanonicalStreamEvent::TextEnd { index: 0 }))
                .and_then(|_| {
                    sink.emit(CanonicalStreamEvent::ResponseCompleted {
                        response_id,
                        finish_reason: "stop".to_string(),
                        usage: CanonicalUsage {
                            input_tokens: 4,
                            output_tokens: 1,
                        },
                    })
                })
                .map_err(M3HubError::Runtime)
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        Box::pin(async move {
            self.state
                .cancelled
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?
                .push(request_id.to_string());
            if self.state.hold_stream.load(Ordering::SeqCst) {
                self.state.stream_release.notify_one();
            }
            if self.state.hold_completion.load(Ordering::SeqCst) {
                self.state.completion_release.notify_one();
            }
            Ok(true)
        })
    }
}

struct MockReconciler {
    state: Arc<MockRuntimeState>,
}

impl M3RuntimeReconciler for MockReconciler {
    fn reconcile<'a>(
        &'a self,
        installed: &'a [M3InstalledModelView],
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<Arc<dyn M3RuntimeDriver>>> {
        Box::pin(async move {
            let mut models = self
                .state
                .models
                .lock()
                .map_err(|_| M3HubError::LockPoisoned)?;
            models.clear();
            for model in installed {
                let active = model
                    .versions
                    .iter()
                    .find(|version| version.active)
                    .ok_or_else(|| M3HubError::State("missing active model".to_string()))?;
                models.insert(
                    model.model_id.clone(),
                    (active.artifact_path.clone(), active.size_bytes),
                );
            }
            drop(models);
            Ok(vec![
                Arc::new(MockRuntimeDriver::new(self.state.clone())) as Arc<dyn M3RuntimeDriver>
            ])
        })
    }
}

#[tokio::test]
async fn reconciled_runtime_load_config_metrics_logs_unload_and_safe_delete_are_wired() {
    let directory = TestDirectory::new("runtime");
    let bytes = payload(90_000, 3);
    let download = Arc::new(MutableDownload::new(bytes.clone(), "etag-runtime"));
    let runtime_state = Arc::new(MockRuntimeState::default());
    let hub = make_hub(
        &directory.0,
        download,
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        Some(Arc::new(MockReconciler {
            state: runtime_state.clone(),
        })),
        None,
    );
    let model = catalog_model(&bytes, "rev-runtime");
    let context = M3OperationContext::default();
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: model.license.declaration_sha256(),
            model: model.clone(),
        },
        &context,
    )
    .await
    .expect("download and reconcile");

    hub.set_runtime_config(&M3SetRuntimeConfigRequest {
        runtime_id: "managed-llama".to_string(),
        values: BTreeMap::from([("threads".to_string(), SettingValue::Integer { value: 8 })]),
    })
    .expect("persist runtime config");
    assert!(hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([("threads".to_string(), SettingValue::Integer { value: 100 })]),
        })
        .is_err());
    hub.load_model(
        &M3LoadModelRequest {
            runtime_id: "managed-llama".to_string(),
            asset_id: model.asset_id(),
            keep_alive: None,
            replace_existing: false,
        },
        &context,
    )
    .await
    .expect("load managed model");
    assert!(matches!(
        hub.runtime_status("managed-llama", &context).await.unwrap(),
        M3RuntimeStatusView::Adapter { running_models, .. } if running_models.len() == 1
    ));
    assert_eq!(
        hub.runtime_logs("managed-llama", 1024, &context)
            .await
            .unwrap()
            .text,
        "ready\ngenerated\n"
    );
    assert!(matches!(
        hub.runtime_metrics("managed-llama", &context)
            .await
            .unwrap(),
        M3RuntimeMetricsView::Adapter { running_models, .. } if running_models.len() == 1
    ));
    assert!(matches!(
        hub.delete_model(
            &M3DeleteModelRequest {
                asset_id: model.asset_id(),
                confirmation: format!("DELETE {}", model.asset_id()),
            },
            &context,
        )
        .await,
        Err(M3HubError::Conflict(_))
    ));
    hub.unload_model(
        &M3UnloadModelRequest {
            runtime_id: "managed-llama".to_string(),
            model_id: model.model_id.clone(),
            force_exact_owner: false,
        },
        &context,
    )
    .await
    .expect("unload");
    assert!(hub
        .delete_model(
            &M3DeleteModelRequest {
                asset_id: model.asset_id(),
                confirmation: format!("DELETE {}", model.asset_id()),
            },
            &context,
        )
        .await
        .expect("delete after unload"));
}

/// `catalog_model` with the model id, display name, and estimated RAM
/// footprint overridden — used to build same/different-family and
/// larger/smaller model pairs for the speculative-decoding draft-model gate
/// tests below, without duplicating every other field `catalog_model`
/// already sets up correctly.
fn family_model(
    bytes: &[u8],
    revision: &str,
    model_id: &str,
    display_name: &str,
    estimated_ram_bytes: u64,
) -> M3CatalogModel {
    let mut model = catalog_model(bytes, revision);
    model.model_id = model_id.to_string();
    model.display_name = display_name.to_string();
    model.estimated_ram_bytes = estimated_ram_bytes;
    model
}

// -- Sampler, Batching, and Speculative Decoding Controls (ROADMAP Phase 8
// item 17) ---------------------------------------------------------------

#[tokio::test]
async fn set_runtime_config_gates_flash_attention_and_mixed_precision_on_hardware() {
    let directory = TestDirectory::new("hardware-gate");
    let bytes = payload(40_000, 11);
    let download = Arc::new(MutableDownload::new(bytes, "etag-hardware-gate"));
    let runtime_state = Arc::new(MockRuntimeState::default());
    let cpu_only_hub = make_hub(
        &directory.0,
        download,
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        None,
        None,
    );

    // A safe default ("auto"/"f16") never needs a GPU and must always save.
    cpu_only_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "flash_attention".to_string(),
                SettingValue::Choice {
                    value: "auto".to_string(),
                },
            )]),
        })
        .expect("auto flash attention never needs gating");
    cpu_only_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "mixed_precision".to_string(),
                SettingValue::Choice {
                    value: "f16".to_string(),
                },
            )]),
        })
        .expect("f16 mixed precision never needs gating");

    // "on"/non-f16 require a real GPU backend; this hub's hardware report is
    // CPU-only, so both must be rejected with a clear reason, not silently
    // accepted.
    let flash_on_rejected = cpu_only_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "flash_attention".to_string(),
                SettingValue::Choice {
                    value: "on".to_string(),
                },
            )]),
        })
        .expect_err("flash attention cannot be forced on without a GPU backend");
    assert!(
        matches!(flash_on_rejected, M3HubError::Unsupported(reason) if reason.contains("GPU backend"))
    );
    let mixed_precision_rejected = cpu_only_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "mixed_precision".to_string(),
                SettingValue::Choice {
                    value: "q8_0".to_string(),
                },
            )]),
        })
        .expect_err("quantized KV cache cannot be enabled without a GPU backend");
    assert!(
        matches!(mixed_precision_rejected, M3HubError::Unsupported(reason) if reason.contains("GPU backend"))
    );

    // The same requests succeed once the Hardware Compatibility report shows
    // a real GPU backend — proving this is a genuine hardware check, not a
    // permanently-closed gate.
    let gpu_directory = TestDirectory::new("hardware-gate-gpu");
    let gpu_bytes = payload(40_000, 13);
    let gpu_download = Arc::new(MutableDownload::new(gpu_bytes, "etag-hardware-gate-gpu"));
    let gpu_runtime_state = Arc::new(MockRuntimeState::default());
    let gpu_hub = make_hub_with_hardware(
        &gpu_directory.0,
        gpu_download,
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(gpu_runtime_state))],
        None,
        None,
        hardware_with_cuda(),
    );
    gpu_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "flash_attention".to_string(),
                SettingValue::Choice {
                    value: "on".to_string(),
                },
            )]),
        })
        .expect("flash attention can be forced on with a CUDA backend available");
    gpu_hub
        .set_runtime_config(&M3SetRuntimeConfigRequest {
            runtime_id: "managed-llama".to_string(),
            values: BTreeMap::from([(
                "mixed_precision".to_string(),
                SettingValue::Choice {
                    value: "q8_0".to_string(),
                },
            )]),
        })
        .expect("quantized KV cache can be enabled with a CUDA backend available");
}

#[tokio::test]
async fn load_model_enforces_the_speculative_decoding_draft_model_gate() {
    let directory = TestDirectory::new("draft-model-gate");
    let download = Arc::new(MutableDownload::new(Vec::new(), "etag-draft"));
    let runtime_state = Arc::new(MockRuntimeState::default());
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        Some(Arc::new(MockReconciler {
            state: runtime_state.clone(),
        })),
        None,
    );
    let context = M3OperationContext::default();

    let target_bytes = payload(60_000, 21);
    let target = family_model(
        &target_bytes,
        "rev-target",
        "llama-3-8b-instruct",
        "Llama 3 8B Instruct",
        8_000_000_000,
    );
    download.set_payload(target_bytes, "etag-target");
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: target.license.declaration_sha256(),
            model: target.clone(),
        },
        &context,
    )
    .await
    .expect("download target model");

    let mismatched_family_bytes = payload(30_000, 23);
    let mismatched_family = family_model(
        &mismatched_family_bytes,
        "rev-mismatch",
        "mistral-7b-instruct",
        "Mistral 7B Instruct",
        4_000_000_000,
    );
    download.set_payload(mismatched_family_bytes, "etag-mismatch");
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: mismatched_family.license.declaration_sha256(),
            model: mismatched_family.clone(),
        },
        &context,
    )
    .await
    .expect("download mismatched-family model");

    // Neither the target itself nor a differently-family model is a valid
    // draft: both must be rejected before the process ever launches.
    hub.set_runtime_config(&M3SetRuntimeConfigRequest {
        runtime_id: "managed-llama".to_string(),
        values: BTreeMap::from([(
            "speculative_decoding_draft_model".to_string(),
            SettingValue::Text {
                value: target.model_id.clone(),
            },
        )]),
    })
    .expect("persisting a not-yet-validated draft choice is allowed");
    let self_as_draft = hub
        .load_model(
            &M3LoadModelRequest {
                runtime_id: "managed-llama".to_string(),
                asset_id: target.asset_id(),
                keep_alive: None,
                replace_existing: false,
            },
            &context,
        )
        .await
        .expect_err("a model cannot be its own speculative-decoding draft");
    assert!(matches!(self_as_draft, M3HubError::Unsupported(_)));

    hub.set_runtime_config(&M3SetRuntimeConfigRequest {
        runtime_id: "managed-llama".to_string(),
        values: BTreeMap::from([(
            "speculative_decoding_draft_model".to_string(),
            SettingValue::Text {
                value: mismatched_family.model_id.clone(),
            },
        )]),
    })
    .expect("persisting a not-yet-validated draft choice is allowed");
    let wrong_family = hub
        .load_model(
            &M3LoadModelRequest {
                runtime_id: "managed-llama".to_string(),
                asset_id: target.asset_id(),
                keep_alive: None,
                replace_existing: false,
            },
            &context,
        )
        .await
        .expect_err("a different-family model is not a compatible draft");
    assert!(matches!(wrong_family, M3HubError::Unsupported(_)));

    // A smaller, same-family model is a genuinely compatible draft.
    let draft_bytes = payload(15_000, 27);
    let draft = family_model(
        &draft_bytes,
        "rev-draft",
        "llama-3-1b-instruct",
        "Llama 3 1B Instruct",
        1_000_000_000,
    );
    download.set_payload(draft_bytes, "etag-draft-model");
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: draft.license.declaration_sha256(),
            model: draft.clone(),
        },
        &context,
    )
    .await
    .expect("download compatible draft model");
    hub.set_runtime_config(&M3SetRuntimeConfigRequest {
        runtime_id: "managed-llama".to_string(),
        values: BTreeMap::from([(
            "speculative_decoding_draft_model".to_string(),
            SettingValue::Text {
                value: draft.model_id.clone(),
            },
        )]),
    })
    .expect("persist a genuinely compatible draft choice");
    hub.load_model(
        &M3LoadModelRequest {
            runtime_id: "managed-llama".to_string(),
            asset_id: target.asset_id(),
            keep_alive: None,
            replace_existing: false,
        },
        &context,
    )
    .await
    .expect("load succeeds once the draft model is a smaller, same-family installed model");
}

#[tokio::test]
async fn resolve_setting_capabilities_reports_gpu_gates_and_draft_model_candidates() {
    let directory = TestDirectory::new("resolve-capabilities");
    let download = Arc::new(MutableDownload::new(Vec::new(), "etag-resolve"));
    let runtime_state = Arc::new(MockRuntimeState::default());
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        Some(Arc::new(MockReconciler {
            state: runtime_state.clone(),
        })),
        None,
    );
    let context = M3OperationContext::default();

    // Before any model is selected, the hardware-only gates already resolve
    // (this hub's hardware is CPU-only) and the model-relative gate reports
    // "select a model" rather than guessing.
    let unscoped = hub
        .resolve_setting_capabilities("managed-llama", None)
        .expect("resolve without a target model");
    let flash_attention = unscoped
        .settings
        .iter()
        .find(|setting| setting.key == "flash_attention")
        .expect("flash_attention present");
    assert!(!flash_attention.supported);
    let draft_model_setting = unscoped
        .settings
        .iter()
        .find(|setting| setting.key == "speculative_decoding_draft_model")
        .expect("speculative_decoding_draft_model present");
    assert!(!draft_model_setting.supported);
    assert!(unscoped.draft_model_candidates.is_empty());

    let target_bytes = payload(60_000, 31);
    let target = family_model(
        &target_bytes,
        "rev-target",
        "qwen2-7b-instruct",
        "Qwen2 7B Instruct",
        7_000_000_000,
    );
    download.set_payload(target_bytes, "etag-target");
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: target.license.declaration_sha256(),
            model: target.clone(),
        },
        &context,
    )
    .await
    .expect("download target model");

    // No compatible draft installed yet: still unsupported, with a specific
    // reason naming the target model.
    let no_draft_yet = hub
        .resolve_setting_capabilities("managed-llama", Some(&target.asset_id()))
        .expect("resolve with a target but no draft installed");
    let draft_model_setting = no_draft_yet
        .settings
        .iter()
        .find(|setting| setting.key == "speculative_decoding_draft_model")
        .expect("speculative_decoding_draft_model present");
    assert!(!draft_model_setting.supported);
    assert!(draft_model_setting
        .unsupported_reason
        .as_deref()
        .is_some_and(|reason| reason.contains(&target.display_name)));
    assert!(no_draft_yet.draft_model_candidates.is_empty());

    let draft_bytes = payload(15_000, 33);
    let draft = family_model(
        &draft_bytes,
        "rev-draft",
        "qwen2-1.5b-instruct",
        "Qwen2 1.5B Instruct",
        1_500_000_000,
    );
    download.set_payload(draft_bytes, "etag-draft");
    hub.download_model(
        &M3DownloadRequest {
            accepted_license_sha256: draft.license.declaration_sha256(),
            model: draft.clone(),
        },
        &context,
    )
    .await
    .expect("download compatible draft model");

    let with_draft = hub
        .resolve_setting_capabilities("managed-llama", Some(&target.asset_id()))
        .expect("resolve with a compatible draft installed");
    let draft_model_setting = with_draft
        .settings
        .iter()
        .find(|setting| setting.key == "speculative_decoding_draft_model")
        .expect("speculative_decoding_draft_model present");
    assert!(draft_model_setting.supported);
    assert!(draft_model_setting.unsupported_reason.is_none());
    assert_eq!(with_draft.draft_model_candidates.len(), 1);
    assert_eq!(
        with_draft.draft_model_candidates[0].model_id,
        draft.model_id
    );
    assert_eq!(
        with_draft.draft_model_candidates[0].display_name,
        draft.display_name
    );
}

struct DeterministicEntropy(Mutex<u8>);

impl LanEntropySource for DeterministicEntropy {
    fn fill(&self, output: &mut [u8]) -> Result<(), String> {
        let mut seed = self.0.lock().map_err(|_| "entropy lock".to_string())?;
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = seed.wrapping_add(index as u8);
        }
        *seed = seed.wrapping_add(29);
        Ok(())
    }
}

struct TestProtector(Vec<u8>);

impl TestProtector {
    fn tag(&self, bytes: &[u8]) -> Vec<u8> {
        let mut hash = Sha256::new();
        hash.update((self.0.len() as u64).to_le_bytes());
        hash.update(&self.0);
        hash.update(bytes);
        hash.finalize().to_vec()
    }
}

impl LanStateProtector for TestProtector {
    fn protector_id(&self) -> &str {
        "test-keychain-v1"
    }

    fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.tag(canonical_state))
    }

    fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
        if self.tag(canonical_state) == tag {
            Ok(())
        } else {
            Err("tag mismatch".to_string())
        }
    }
}

struct VecFrameSink(Vec<little_monkey_lib::compatibility_hub::ProtocolStreamFrame>);

impl M3ProtocolFrameSink for VecFrameSink {
    fn emit(
        &mut self,
        frame: little_monkey_lib::compatibility_hub::ProtocolStreamFrame,
    ) -> Result<(), String> {
        self.0.push(frame);
        Ok(())
    }
}

fn pair_scoped_token(
    hub: &M3RuntimeHub,
    label: &str,
    scopes: BTreeSet<ApiScope>,
    allowed_models: BTreeSet<String>,
    now_ms: u64,
) -> PairedToken {
    let challenge = hub
        .begin_pairing(
            PairingRequest {
                client_label: label.to_string(),
                scopes,
                backends: BTreeSet::from([ApiBackend::ManagedLocal]),
                allowed_models,
                token_expires_at_ms: Some(now_ms + 100_000),
            },
            now_ms,
            "127.0.0.1",
        )
        .expect("begin scoped pairing");
    hub.complete_pairing(
        &challenge.challenge_id,
        &challenge.pairing_code,
        now_ms + 1,
        "127.0.0.1",
    )
    .expect("complete scoped pairing")
}

#[tokio::test]
async fn scoped_pairing_dispatch_stream_rate_limit_cancel_revoke_and_audit_are_wired() {
    let directory = TestDirectory::new("lan-api");
    let runtime_state = Arc::new(MockRuntimeState::default());
    let lan_factory = Arc::new(DefaultM3LanAccessFactory::new(
        Arc::new(DeterministicEntropy(Mutex::new(5))),
        Arc::new(TestProtector(b"test-secret-key".to_vec())),
    ));
    let hub = make_hub(
        &directory.0,
        Arc::new(MutableDownload::new(payload(70_000, 1), "unused")),
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        None,
        Some(lan_factory),
    );
    let mut policy = LanServerPolicy::default();
    policy.rate_limit = RateLimitPolicy {
        window_ms: 60_000,
        max_requests: 2,
        max_input_bytes: 1024 * 1024,
    };
    policy.tls = TlsPolicy::Disabled;
    hub.configure_lan(policy.clone())
        .expect("configure loopback LAN");
    let challenge = hub
        .begin_pairing(
            PairingRequest {
                client_label: "Test client".to_string(),
                scopes: BTreeSet::from([ApiScope::ChatCompletions]),
                backends: BTreeSet::from([ApiBackend::ManagedLocal]),
                allowed_models: BTreeSet::from(["local-model".to_string()]),
                token_expires_at_ms: Some(100_000),
            },
            20_000,
            "127.0.0.1",
        )
        .expect("pairing challenge");
    let paired = hub
        .complete_pairing(
            &challenge.challenge_id,
            &challenge.pairing_code,
            20_001,
            "127.0.0.1",
        )
        .expect("paired token");
    assert!(!serde_json::to_string(&hub.list_tokens().unwrap())
        .unwrap()
        .contains(&paired.token));

    let body = serde_json::to_vec(&json!({
        "model":"local-model",
        "messages":[{"role":"user","content":"hello"}],
        "max_tokens":32
    }))
    .unwrap();
    let caller = M3ApiCaller::External {
        bearer_token: paired.token.clone(),
        remote_address: "127.0.0.1".to_string(),
    };
    let context = M3OperationContext::default();
    let response = hub
        .dispatch_api(
            &M3ApiDispatchRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: "request-1".to_string(),
                body: body.clone(),
                caller: caller.clone(),
                now_ms: 20_002,
            },
            &context,
        )
        .await
        .expect("authorized completion");
    assert_eq!(response.body["object"], "chat.completion");

    let stream_body = serde_json::to_vec(&json!({
        "model":"local-model",
        "messages":[{"role":"user","content":"hello"}],
        "max_tokens":32,
        "stream":true
    }))
    .unwrap();
    let mut frames = VecFrameSink(Vec::new());
    hub.dispatch_api_stream(
        &M3ApiDispatchRequest {
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            runtime_id: "managed-llama".to_string(),
            request_id: "request-2".to_string(),
            body: stream_body,
            caller: caller.clone(),
            now_ms: 20_003,
        },
        &mut frames,
        &context,
    )
    .await
    .expect("authorized stream");
    assert_eq!(frames.0.last().expect("terminal frame").data, "[DONE]");

    assert!(matches!(
        hub.dispatch_api(
            &M3ApiDispatchRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: "request-3".to_string(),
                body,
                caller: caller.clone(),
                now_ms: 20_004,
            },
            &context,
        )
        .await,
        Err(M3HubError::RateLimited { .. })
    ));

    assert!(matches!(
        hub.cancel_inference(
            &M3CancelInferenceRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: "request-internal-cancel".to_string(),
                model_id: "local-model".to_string(),
                caller: M3ApiCaller::Internal,
                now_ms: 20_005,
            },
            &context,
        )
        .await,
        Err(M3HubError::NotFound(_))
    ));
    assert!(runtime_state.cancelled.lock().unwrap().is_empty());

    hub.revoke_token(&paired.record.token_id, 20_006, "127.0.0.1")
        .expect("revoke");
    let revoked_body = serde_json::to_vec(&json!({
        "model":"local-model",
        "messages":[{"role":"user","content":"after revoke"}],
        "max_tokens":8
    }))
    .unwrap();
    // Generic `Unauthorized`, not `Forbidden("token is revoked")`: a revoked
    // token must be indistinguishable from one that was never issued, or the
    // response is an existence oracle. See `compatibility_hub.rs`'s
    // `credential_validity_denial`.
    assert!(matches!(
        hub.dispatch_api(
            &M3ApiDispatchRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: "request-revoked".to_string(),
                body: revoked_body,
                caller,
                now_ms: 20_007,
            },
            &context,
        )
        .await,
        Err(M3HubError::Unauthorized(_))
    ));
    let audit = hub.security_audit_events().expect("audit");
    assert!(audit
        .iter()
        .any(|event| event.kind == SecurityAuditKind::TokenAuthorized));
    assert!(audit
        .iter()
        .any(|event| event.kind == SecurityAuditKind::TokenRateLimited));
    assert!(audit
        .iter()
        .any(|event| event.kind == SecurityAuditKind::TokenRevoked));
    assert!(audit
        .iter()
        .all(|event| !event.detail.contains(&paired.token)));

    let delete_challenge = hub
        .begin_pairing(
            PairingRequest {
                client_label: "Lifecycle client".to_string(),
                scopes: BTreeSet::from([ApiScope::ModelDelete]),
                backends: BTreeSet::from([ApiBackend::ManagedLocal]),
                allowed_models: BTreeSet::from(["local-model".to_string()]),
                token_expires_at_ms: Some(100_000),
            },
            20_008,
            "127.0.0.1",
        )
        .expect("lifecycle pairing challenge");
    let delete_token = hub
        .complete_pairing(
            &delete_challenge.challenge_id,
            &delete_challenge.pairing_code,
            20_009,
            "127.0.0.1",
        )
        .expect("lifecycle token");
    let mut authorization = M3ExternalOperationAuthorization {
        bearer_token: delete_token.token.clone(),
        scope: ApiScope::ModelDelete,
        backend: ApiBackend::ManagedLocal,
        model_id: Some("local-model".to_string()),
        input_bytes: 0,
        remote_address: "127.0.0.1".to_string(),
        destructive_confirmation: Some("DELETE wrong-model".to_string()),
        now_ms: 20_010,
    };
    assert!(matches!(
        hub.authorize_external_operation(&authorization),
        Err(M3HubError::Forbidden(_))
    ));
    authorization.destructive_confirmation = Some("DELETE local-model".to_string());
    authorization.now_ms = 20_011;
    assert_eq!(
        hub.authorize_external_operation(&authorization)
            .expect("authorize exact destructive lifecycle request")
            .scope,
        ApiScope::ModelDelete
    );

    assert!(hub
        .disable_lan("DISABLE LAN API")
        .expect("disable LAN and revoke live tokens"));
    hub.configure_lan(policy).expect("re-enable loopback LAN");
    authorization.now_ms = 20_012;
    // `disable_lan` revoked it, and a revoked token now answers the same
    // generic `Unauthorized` an unknown one does — still a refusal, just one
    // that no longer confirms the token was ever real.
    assert!(matches!(
        hub.authorize_external_operation(&authorization),
        Err(M3HubError::Unauthorized(_))
    ));
}

#[tokio::test]
async fn cancellation_is_bound_to_active_request_metadata_and_paired_principal() {
    let directory = TestDirectory::new("cancel-ownership");
    let runtime_state = Arc::new(MockRuntimeState::default());
    runtime_state.hold_stream.store(true, Ordering::SeqCst);
    let lan_factory = Arc::new(DefaultM3LanAccessFactory::new(
        Arc::new(DeterministicEntropy(Mutex::new(50))),
        Arc::new(TestProtector(b"cancel-ownership-key".to_vec())),
    ));
    let hub = Arc::new(make_hub(
        &directory.0,
        Arc::new(MutableDownload::new(payload(70_000, 1), "unused")),
        Vec::new(),
        vec![Arc::new(MockRuntimeDriver::new(runtime_state.clone()))],
        None,
        Some(lan_factory),
    ));
    let mut policy = LanServerPolicy::default();
    policy.rate_limit = RateLimitPolicy {
        window_ms: 60_000,
        max_requests: 100,
        max_input_bytes: 16 * 1024 * 1024,
    };
    policy.tls = TlsPolicy::Disabled;
    hub.configure_lan(policy)
        .expect("configure cancellation LAN");

    let token_a = pair_scoped_token(
        &hub,
        "owner-a",
        BTreeSet::from([ApiScope::ChatCompletions]),
        BTreeSet::from(["local-model".to_string()]),
        30_000,
    );
    let token_b = pair_scoped_token(
        &hub,
        "owner-b-same-capabilities",
        BTreeSet::from([ApiScope::ChatCompletions]),
        BTreeSet::from(["local-model".to_string()]),
        30_010,
    );
    let wrong_model_token = pair_scoped_token(
        &hub,
        "wrong-model",
        BTreeSet::from([ApiScope::ChatCompletions]),
        BTreeSet::from(["other-model".to_string()]),
        30_020,
    );
    let wrong_scope_token = pair_scoped_token(
        &hub,
        "wrong-scope",
        BTreeSet::from([ApiScope::Responses]),
        BTreeSet::from(["local-model".to_string()]),
        30_030,
    );
    let stream_body = serde_json::to_vec(&json!({
        "model":"local-model",
        "messages":[{"role":"user","content":"hold"}],
        "max_tokens":32,
        "stream":true
    }))
    .unwrap();
    let request_id = "owned-stream";
    let owner_token = token_a.token.clone();
    let stream_owner_token = owner_token.clone();
    let stream_hub = hub.clone();
    let stream_task = tokio::spawn(async move {
        let mut frames = VecFrameSink(Vec::new());
        let result = stream_hub
            .dispatch_api_stream(
                &M3ApiDispatchRequest {
                    protocol: CompatibilityProtocol::OpenAiChatCompletions,
                    runtime_id: "managed-llama".to_string(),
                    request_id: request_id.to_string(),
                    body: stream_body,
                    caller: M3ApiCaller::External {
                        bearer_token: stream_owner_token,
                        remote_address: "127.0.0.1".to_string(),
                    },
                    now_ms: 30_100,
                },
                &mut frames,
                &M3OperationContext::default(),
            )
            .await;
        (result, frames)
    });
    runtime_state.stream_started.notified().await;

    let duplicate_body = serde_json::to_vec(&json!({
        "model":"local-model",
        "messages":[{"role":"user","content":"duplicate"}],
        "max_tokens":32,
        "stream":true
    }))
    .unwrap();
    let mut duplicate_frames = VecFrameSink(Vec::new());
    assert!(matches!(
        hub.dispatch_api_stream(
            &M3ApiDispatchRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: request_id.to_string(),
                body: duplicate_body,
                caller: M3ApiCaller::External {
                    bearer_token: token_b.token.clone(),
                    remote_address: "127.0.0.1".to_string(),
                },
                now_ms: 30_101,
            },
            &mut duplicate_frames,
            &M3OperationContext::default(),
        )
        .await,
        Err(M3HubError::Conflict(_))
    ));

    let cancel = |token: String,
                  protocol: CompatibilityProtocol,
                  model_id: &str,
                  target_request_id: &str,
                  now_ms: u64| M3CancelInferenceRequest {
        protocol,
        runtime_id: "managed-llama".to_string(),
        request_id: target_request_id.to_string(),
        model_id: model_id.to_string(),
        caller: M3ApiCaller::External {
            bearer_token: token,
            remote_address: "127.0.0.1".to_string(),
        },
        now_ms,
    };
    let missing = hub
        .cancel_inference(
            &cancel(
                token_b.token.clone(),
                CompatibilityProtocol::OpenAiChatCompletions,
                "local-model",
                "missing-stream",
                30_102,
            ),
            &M3OperationContext::default(),
        )
        .await
        .expect_err("missing request must stay concealed");
    let other_owner = hub
        .cancel_inference(
            &cancel(
                token_b.token.clone(),
                CompatibilityProtocol::OpenAiChatCompletions,
                "local-model",
                request_id,
                30_103,
            ),
            &M3OperationContext::default(),
        )
        .await
        .expect_err("same-capability foreign token must not cancel");
    assert!(matches!(missing, M3HubError::NotFound(_)));
    assert!(matches!(other_owner, M3HubError::NotFound(_)));
    assert_eq!(missing.to_string(), other_owner.to_string());

    assert!(matches!(
        hub.cancel_inference(
            &cancel(
                wrong_model_token.token,
                CompatibilityProtocol::OpenAiChatCompletions,
                "other-model",
                request_id,
                30_104,
            ),
            &M3OperationContext::default(),
        )
        .await,
        Err(M3HubError::NotFound(_))
    ));
    assert!(matches!(
        hub.cancel_inference(
            &cancel(
                wrong_scope_token.token,
                CompatibilityProtocol::OpenAiResponses,
                "local-model",
                request_id,
                30_105,
            ),
            &M3OperationContext::default(),
        )
        .await,
        Err(M3HubError::NotFound(_))
    ));
    assert!(runtime_state.cancelled.lock().unwrap().is_empty());

    assert!(matches!(
        hub.cancel_inference(
            &M3CancelInferenceRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "other-runtime".to_string(),
                request_id: request_id.to_string(),
                model_id: "local-model".to_string(),
                caller: M3ApiCaller::External {
                    bearer_token: owner_token.clone(),
                    remote_address: "127.0.0.1".to_string(),
                },
                now_ms: 30_106,
            },
            &M3OperationContext::default(),
        )
        .await,
        Err(M3HubError::NotFound(_))
    ));
    assert!(runtime_state.cancelled.lock().unwrap().is_empty());

    assert!(hub
        .cancel_inference(
            &cancel(
                owner_token,
                CompatibilityProtocol::OpenAiChatCompletions,
                "local-model",
                request_id,
                30_107,
            ),
            &M3OperationContext::default(),
        )
        .await
        .expect("exact paired owner cancellation"));
    stream_task
        .await
        .expect("owned stream task")
        .0
        .expect("cancelled owned stream exits");
    assert_eq!(
        runtime_state.cancelled.lock().unwrap().as_slice(),
        ["owned-stream"]
    );

    runtime_state.hold_stream.store(false, Ordering::SeqCst);
    hub.dispatch_api(
        &M3ApiDispatchRequest {
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            runtime_id: "managed-llama".to_string(),
            request_id: request_id.to_string(),
            body: serde_json::to_vec(&json!({
                "model":"local-model",
                "messages":[{"role":"user","content":"reused after completion"}],
                "max_tokens":8
            }))
            .unwrap(),
            caller: M3ApiCaller::Internal,
            now_ms: 30_108,
        },
        &M3OperationContext::default(),
    )
    .await
    .expect("completed requestId may be reused after RAII cleanup");
    runtime_state.hold_completion.store(true, Ordering::SeqCst);
    let completion_hub = hub.clone();
    let completion_task = tokio::spawn(async move {
        completion_hub
            .dispatch_api(
                &M3ApiDispatchRequest {
                    protocol: CompatibilityProtocol::OpenAiChatCompletions,
                    runtime_id: "managed-llama".to_string(),
                    request_id: "owned-completion".to_string(),
                    body: serde_json::to_vec(&json!({
                        "model":"local-model",
                        "messages":[{"role":"user","content":"hold completion"}],
                        "max_tokens":8
                    }))
                    .unwrap(),
                    caller: M3ApiCaller::Internal,
                    now_ms: 30_200,
                },
                &M3OperationContext::default(),
            )
            .await
    });
    runtime_state.completion_started.notified().await;
    assert!(hub
        .cancel_inference(
            &M3CancelInferenceRequest {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                runtime_id: "managed-llama".to_string(),
                request_id: "owned-completion".to_string(),
                model_id: "local-model".to_string(),
                caller: M3ApiCaller::Internal,
                now_ms: 30_201,
            },
            &M3OperationContext::default(),
        )
        .await
        .expect("exact internal completion cancellation"));
    completion_task
        .await
        .expect("completion task")
        .expect("cancelled completion exits");
    assert_eq!(
        runtime_state.cancelled.lock().unwrap().as_slice(),
        ["owned-stream", "owned-completion"]
    );
}

#[test]
fn catalog_model_manifest_stays_backward_compatible_without_new_provenance_fields() {
    let bytes = payload(4_096, 3);
    let model = catalog_model(&bytes, "rev-compat");
    let mut value = serde_json::to_value(&model).expect("serialize catalog model");
    let object = value
        .as_object_mut()
        .expect("catalog model is a JSON object");
    // Simulate a manifest written before template/projector/provenance
    // existed: the keys are entirely absent, not merely null.
    assert!(object.remove("template").is_some());
    assert!(object.remove("projector").is_some());
    assert!(object.remove("catalogRetrievedAtMs").is_some());

    let restored: M3CatalogModel = serde_json::from_value(value)
        .expect("a legacy manifest without the new fields must still deserialize");
    assert_eq!(restored.template, None);
    assert_eq!(restored.projector, None);
    assert_eq!(restored.catalog_retrieved_at_ms, None);
    restored
        .validate()
        .expect("a legacy manifest without the new fields must still validate");
}

#[tokio::test]
async fn search_stamps_provenance_and_installed_view_surfaces_template_projector_and_source() {
    let bytes = payload(2_048, 11);
    let mut model = catalog_model(&bytes, "rev-1");
    model.template = Some("chatml".to_string());
    model.projector = Some(M3ProjectorRef {
        kind: "clip".to_string(),
        sha256: sha256(b"projector-bytes"),
        size_bytes: 4_096,
    });
    let directory = TestDirectory::new("provenance");
    let download = Arc::new(MutableDownload::new(bytes.clone(), "etag-provenance"));
    let hub = make_hub(
        &directory.0,
        download,
        vec![Arc::new(StaticCatalog {
            source_id: "test-catalog".to_string(),
            entries: vec![model],
        })],
        Vec::new(),
        None,
        None,
    );
    let context = M3OperationContext::new(10_000);

    let matches = hub
        .search_catalog("Local Model", 10, &context)
        .await
        .expect("search catalog");
    assert_eq!(matches.len(), 1);
    let retrieved_at = matches[0]
        .model
        .catalog_retrieved_at_ms
        .expect("search stamps a local retrieval timestamp");
    assert!(retrieved_at > 0);

    let request = M3DownloadRequest {
        accepted_license_sha256: matches[0].model.license.declaration_sha256(),
        model: matches[0].model.clone(),
    };
    let installed = hub
        .download_model(&request, &context)
        .await
        .expect("install stamped model");
    let active = installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version");
    assert_eq!(active.source_id, "test-catalog");
    assert_eq!(active.template.as_deref(), Some("chatml"));
    assert_eq!(
        active
            .projector
            .as_ref()
            .map(|projector| projector.kind.as_str()),
        Some("clip")
    );
    assert_eq!(active.catalog_retrieved_at_ms, Some(retrieved_at));
}

/// ROADMAP Phase 8 item 12 (Multimodal Projector and Vision Model Manager):
/// a model declared vision-capable is only ever shown `vision_ready` once a
/// real, digest-verified projector backs it — never merely because the
/// catalog set `capabilities.vision = true`. This exercises every state:
/// missing reference, declared-but-unverified, a failed verification
/// (digest mismatch), and a real successful verification.
#[tokio::test]
async fn vision_capable_model_surfaces_missing_and_verified_projector_evidence() {
    let directory = TestDirectory::new("projector-evidence");
    let bytes = payload(4_096, 21);
    let mut model = catalog_model(&bytes, "rev-vision");
    model.capabilities.vision = true;
    model.projector = None; // declared vision-capable, but no projector reference at all yet
    let download = Arc::new(MutableDownload::new(bytes.clone(), "etag-vision"));
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let context = M3OperationContext::new(10_000);

    let request = M3DownloadRequest {
        accepted_license_sha256: model.license.declaration_sha256(),
        model: model.clone(),
    };
    let installed = hub
        .download_model(&request, &context)
        .await
        .expect("install vision-capable model without a projector reference yet");
    let active = installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version");
    assert_eq!(
        active.projector_verification,
        M3ProjectorVerificationState::MissingReference
    );
    assert!(
        !active.vision_ready,
        "vision must never be ready with no projector reference at all"
    );
    assert_eq!(active.estimated_projector_memory_bytes, None);

    // Now the catalog ships a new revision that declares a real projector
    // reference — still unverified until real bytes are checked. A distinct
    // revision/byte payload (rather than re-declaring the exact same
    // version) is used deliberately so this goes through the full
    // download+state-mutation path instead of `download_model`'s
    // identical-version fast path (keyed only on asset/version/sha256,
    // which would otherwise return the earlier stored entry unchanged).
    let v2_bytes = payload(4_096, 22);
    let projector_bytes = payload(2_048, 77);
    let mut with_projector = catalog_model(&v2_bytes, "rev-vision-v2");
    with_projector.capabilities.vision = true;
    with_projector.projector = Some(M3ProjectorRef {
        kind: "clip".to_string(),
        sha256: sha256(&projector_bytes),
        size_bytes: projector_bytes.len() as u64,
    });
    download.set_payload(v2_bytes.clone(), "etag-vision-v2");
    let request = M3DownloadRequest {
        accepted_license_sha256: with_projector.license.declaration_sha256(),
        model: with_projector.clone(),
    };
    let installed = hub
        .download_model(&request, &context)
        .await
        .expect("install a new revision that declares a projector reference");
    let active = installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version");
    assert_eq!(
        active.projector_verification,
        M3ProjectorVerificationState::Unverified
    );
    assert!(
        !active.vision_ready,
        "an unverified projector must never be shown vision-ready"
    );
    assert_eq!(
        active.estimated_projector_memory_bytes,
        Some(projector_bytes.len() as u64)
    );

    // A digest mismatch is rejected outright and never marks anything verified.
    let projector_file = directory.0.join("wrong-projector.bin");
    fs::write(&projector_file, payload(2_048, 99)).expect("write wrong candidate projector");
    let mismatch = hub
        .verify_projector(
            &M3VerifyProjectorRequest {
                asset_id: installed.asset_id.clone(),
                version_key: active.version_key.clone(),
                candidate_path: projector_file,
            },
            &context,
        )
        .await;
    assert!(matches!(mismatch, Err(M3HubError::Integrity { .. })));

    // The real bytes verify successfully and promote this version to
    // genuinely vision-ready (LlamaCpp is one of the runtime kinds whose
    // outbound wire composition carries an image block today).
    let projector_file = directory.0.join("real-projector.bin");
    fs::write(&projector_file, &projector_bytes).expect("write real candidate projector");
    let verified = hub
        .verify_projector(
            &M3VerifyProjectorRequest {
                asset_id: installed.asset_id.clone(),
                version_key: active.version_key.clone(),
                candidate_path: projector_file,
            },
            &context,
        )
        .await
        .expect("verify real projector bytes");
    let active = verified
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version");
    assert_eq!(
        active.projector_verification,
        M3ProjectorVerificationState::Verified
    );
    assert!(active.projector_verified_at_ms.is_some());
    assert!(
        active.vision_ready,
        "a verified projector on a transport-capable runtime must be vision-ready"
    );

    // Verifying a model version with no projector reference at all is a
    // clear NotFound, not a silent no-op or a false success.
    let mut no_projector_model = catalog_model(&payload(4_096, 5), "rev-no-projector");
    no_projector_model.capabilities.vision = false;
    let request = M3DownloadRequest {
        accepted_license_sha256: no_projector_model.license.declaration_sha256(),
        model: no_projector_model.clone(),
    };
    let other_directory = TestDirectory::new("no-projector");
    let other_download = Arc::new(MutableDownload::new(payload(4_096, 5), "etag-no-projector"));
    let other_hub = make_hub(
        &other_directory.0,
        other_download,
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let other_installed = other_hub
        .download_model(&request, &context)
        .await
        .expect("install a non-vision model");
    let other_active = other_installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active");
    assert_eq!(
        other_active.projector_verification,
        M3ProjectorVerificationState::NotRequired
    );
    let missing_projector = other_hub
        .verify_projector(
            &M3VerifyProjectorRequest {
                asset_id: other_installed.asset_id.clone(),
                version_key: other_active.version_key.clone(),
                candidate_path: directory.0.join("irrelevant.bin"),
            },
            &context,
        )
        .await;
    assert!(matches!(missing_projector, Err(M3HubError::NotFound(_))));
}

#[tokio::test]
async fn identical_payload_across_assets_is_reused_without_a_network_transfer_and_survives_donor_deletion(
) {
    let directory = TestDirectory::new("dedup");
    let shared_bytes = payload(120_000, 42);
    let download = Arc::new(MutableDownload::new(shared_bytes.clone(), "etag-shared"));
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let context = M3OperationContext::new(10_000);

    let donor = catalog_model(&shared_bytes, "rev-donor");
    let donor_request = M3DownloadRequest {
        accepted_license_sha256: donor.license.declaration_sha256(),
        model: donor.clone(),
    };
    let installed_donor = hub
        .download_model(&donor_request, &context)
        .await
        .expect("install donor over the network");
    assert!(!download.offsets().is_empty());

    // A different variant with byte-identical content must reuse the
    // donor's verified payload instead of hitting the network again.
    let before = download.offsets().len();
    let mut reuser = donor.clone();
    reuser.variant_id = "q5_k_m".to_string();
    reuser.revision = "rev-reuser".to_string();
    let reuser_request = M3DownloadRequest {
        accepted_license_sha256: reuser.license.declaration_sha256(),
        model: reuser.clone(),
    };
    let installed_reuser = hub
        .download_model(&reuser_request, &context)
        .await
        .expect("reuse the donor's verified payload");
    assert_eq!(
        download.offsets().len(),
        before,
        "a byte-identical variant must not trigger any additional network range reads"
    );
    assert_ne!(installed_donor.asset_id, installed_reuser.asset_id);
    let reuser_path = installed_reuser
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active reuser version")
        .artifact_path
        .clone();
    assert_eq!(fs::read(&reuser_path).unwrap(), shared_bytes);

    // Deleting the donor must not corrupt the reused survivor: the shared
    // bytes must remain intact and independently owned on disk.
    assert!(hub
        .delete_model(
            &M3DeleteModelRequest {
                asset_id: donor.asset_id(),
                confirmation: format!("DELETE {}", donor.asset_id()),
            },
            &context,
        )
        .await
        .expect("delete donor"));
    assert!(hub
        .list_installed_models()
        .unwrap()
        .iter()
        .any(|installed| installed.asset_id == reuser.asset_id()));
    assert_eq!(fs::read(&reuser_path).unwrap(), shared_bytes);
}

#[tokio::test]
async fn corrupted_local_candidate_is_never_reused_and_falls_back_to_a_real_download() {
    let directory = TestDirectory::new("dedup-corrupt");
    let shared_bytes = payload(80_000, 5);
    let download = Arc::new(MutableDownload::new(
        shared_bytes.clone(),
        "etag-corrupt-guard",
    ));
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let context = M3OperationContext::new(10_000);

    let donor = catalog_model(&shared_bytes, "rev-donor");
    let donor_request = M3DownloadRequest {
        accepted_license_sha256: donor.license.declaration_sha256(),
        model: donor.clone(),
    };
    let installed_donor = hub
        .download_model(&donor_request, &context)
        .await
        .expect("install donor");
    let donor_path = installed_donor
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active donor version")
        .artifact_path
        .clone();

    // Corrupt the donor payload on disk without changing its length, e.g.
    // simulating bit rot the hub did not itself cause.
    let mut corrupted = shared_bytes.clone();
    corrupted[0] ^= 0xFF;
    fs::write(&donor_path, &corrupted).expect("corrupt donor payload in place");

    let before = download.offsets().len();
    let mut reuser = donor.clone();
    reuser.variant_id = "q5_k_m".to_string();
    reuser.revision = "rev-reuser".to_string();
    let reuser_request = M3DownloadRequest {
        accepted_license_sha256: reuser.license.declaration_sha256(),
        model: reuser.clone(),
    };
    let installed_reuser = hub
        .download_model(&reuser_request, &context)
        .await
        .expect("fall back to a genuine download");
    assert!(
        download.offsets().len() > before,
        "a corrupted local candidate must never be reused; a real download must occur instead"
    );
    let reuser_path = installed_reuser
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active reuser version")
        .artifact_path
        .clone();
    assert_eq!(fs::read(&reuser_path).unwrap(), shared_bytes);
}

#[tokio::test]
async fn transport_corruption_is_caught_by_the_final_digest_check_and_a_retry_recovers() {
    let directory = TestDirectory::new("transport-corruption");
    let bytes = payload(50_000, 9);
    let download = Arc::new(MutableDownload::new(bytes.clone(), "etag-corrupt-guard"));
    download.corrupt_chunk_at(0);
    let hub = make_hub(
        &directory.0,
        download.clone(),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let model = catalog_model(&bytes, "rev-1");
    let context = M3OperationContext::new(10_000);
    let request = M3DownloadRequest {
        accepted_license_sha256: model.license.declaration_sha256(),
        model: model.clone(),
    };

    // A structurally valid but bit-flipped chunk must survive framing
    // checks yet still be caught by the whole-file digest verification, and
    // must never be silently accepted or partially published.
    assert!(matches!(
        hub.download_model(&request, &context).await,
        Err(M3HubError::Integrity { .. })
    ));
    assert!(hub.list_installed_models().unwrap().is_empty());

    // A retry must not resume the corrupted bytes; it must restart cleanly
    // and succeed once the transport stops corrupting the stream.
    let installed = hub
        .download_model(&request, &context)
        .await
        .expect("retry recovers with clean bytes");
    let active_path = installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version")
        .artifact_path
        .clone();
    assert_eq!(fs::read(&active_path).unwrap(), bytes);
}

/// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
/// `M3RuntimeHub::model_staleness_check` reuses `search_catalog` (the same
/// mechanism `RuntimeHubModels.tsx`'s "Find updates" button already drives)
/// to detect a newer catalog revision, then only flags it once the installed
/// version has also gone unrefreshed for a long time. This exercises the
/// full pipeline — not just the pure comparison in `model_retirement.rs`'s
/// own unit tests — with a controllable clock standing in for real time
/// passing between install and check.
#[tokio::test]
async fn model_staleness_check_flags_a_long_unrefreshed_model_once_a_newer_revision_exists() {
    let directory = TestDirectory::new("staleness-flagged");
    let bytes_rev1 = payload(2_048, 21);
    let mut model_rev1 = catalog_model(&bytes_rev1, "rev-1");
    // `StaticCatalog::search` (this fixture) matches on `display_name`, and
    // `model_staleness_check` queries by `model_id` — matching production's
    // own "Find updates" query shape (`searchCatalog(model.modelId)` in
    // `RuntimeHubModels.tsx`), which a real catalog source is expected to
    // resolve. Give both revisions a display name containing the model id so
    // this test fixture resolves the same query shape.
    model_rev1.display_name = "local-model rev1".to_string();
    let clock = Arc::new(ControllableClock::new(10_000));
    let hub = M3RuntimeHub::new(
        &directory.0,
        test_config(),
        M3RuntimeHubDependencies {
            clock: clock.clone() as Arc<dyn M3Clock>,
            hardware: Arc::new(FixedHardware(hardware())),
            download: Arc::new(MutableDownload::new(bytes_rev1.clone(), "etag-rev1")),
            catalogs: vec![Arc::new(StaticCatalog {
                source_id: "test-catalog".to_string(),
                entries: vec![model_rev1.clone()],
            })],
            runtimes: Vec::new(),
            runtime_reconciler: None,
            lan_factory: None,
        },
    )
    .expect("M3 hub");
    let context = M3OperationContext::new(10_000);

    let matches = hub
        .search_catalog("local-model", 10, &context)
        .await
        .expect("search catalog for rev-1");
    assert_eq!(matches.len(), 1);
    let installed = hub
        .download_model(
            &M3DownloadRequest {
                accepted_license_sha256: matches[0].model.license.declaration_sha256(),
                model: matches[0].model.clone(),
            },
            &context,
        )
        .await
        .expect("install rev-1");
    let asset_id = installed.asset_id.clone();
    let installed_at_ms = installed
        .versions
        .iter()
        .find(|version| version.active)
        .expect("active version")
        .installed_at_ms;
    assert_eq!(
        installed_at_ms, 10_000,
        "controllable clock is exact, not auto-incrementing"
    );

    // Nothing to migrate to yet: still on the only revision the catalog
    // knows about, however old.
    clock.set(10_000 + STALE_LOCAL_MODEL_THRESHOLD_MS + 1);
    assert_eq!(
        hub.model_staleness_check(&asset_id, &context)
            .await
            .expect("staleness check"),
        None,
        "no newer catalog revision exists yet — nothing to flag"
    );

    // The catalog moves on to a new revision of the same model/variant/source.
    let bytes_rev2 = payload(2_048, 22);
    let mut model_rev2 = catalog_model(&bytes_rev2, "rev-2");
    model_rev2.display_name = "local-model rev2".to_string();
    hub.replace_catalog_sources(vec![Arc::new(StaticCatalog {
        source_id: "test-catalog".to_string(),
        entries: vec![model_rev2],
    })])
    .expect("swap in a newer catalog revision");

    // Freshly installed relative to the new clock value — not stale yet even
    // though a newer revision now exists.
    clock.set(installed_at_ms + STALE_LOCAL_MODEL_THRESHOLD_MS - 1);
    assert_eq!(
        hub.model_staleness_check(&asset_id, &context)
            .await
            .expect("staleness check"),
        None,
        "a newer revision exists, but the install isn't old enough yet"
    );

    // Enough time has passed *and* a newer revision exists — now it's flagged.
    clock.set(installed_at_ms + STALE_LOCAL_MODEL_THRESHOLD_MS + 1);
    let warning = hub
        .model_staleness_check(&asset_id, &context)
        .await
        .expect("staleness check")
        .expect("should be flagged as stale");
    assert_eq!(warning.asset_id, asset_id);
    assert_eq!(warning.installed_revision, "rev-1");
    assert_eq!(warning.latest_revision, "rev-2");
    assert_eq!(
        warning.suggested_replacement_display_name,
        "local-model rev2"
    );
    assert!(warning.age_ms >= STALE_LOCAL_MODEL_THRESHOLD_MS);
}

#[tokio::test]
async fn model_staleness_check_rejects_an_unknown_asset_id() {
    let directory = TestDirectory::new("staleness-unknown-asset");
    let hub = make_hub(
        &directory.0,
        Arc::new(MutableDownload::new(Vec::new(), "etag")),
        Vec::new(),
        Vec::new(),
        None,
        None,
    );
    let context = M3OperationContext::new(10_000);
    assert!(matches!(
        hub.model_staleness_check("does-not-exist", &context).await,
        Err(M3HubError::NotFound(_))
    ));
}
