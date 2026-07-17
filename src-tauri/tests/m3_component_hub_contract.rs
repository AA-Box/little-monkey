//! Contract tests for the Runtime Component Update Channels system
//! (`M3ComponentHub` in `m3_runtime_hub.rs`). These mirror the model
//! manifest/blob/digest store's own contract tests
//! (`m3_runtime_hub_contract.rs`) since components deliberately reuse that
//! system's shape: resumable/verified downloads, activate-to-roll-back, and
//! bounded version retention — but exercised against the independent
//! component storage root and state.

use little_monkey_lib::m3_runtime_hub::*;
use little_monkey_lib::runtime_adapter::AcceleratorKind;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "m3-component-hub-{label}-{}-{}",
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

struct DownloadState {
    bytes: Vec<u8>,
    etag: String,
    fail_once_at: Option<u64>,
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
            Ok(M3DownloadChunk {
                offset,
                total_bytes: state.bytes.len() as u64,
                etag: Some(state.etag.clone()),
                bytes: state.bytes[start..end].to_vec(),
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

#[allow(clippy::too_many_arguments)]
fn entry(
    bytes: &[u8],
    component_id: &str,
    version: &str,
    channel: M3ComponentChannel,
    published_at_ms: u64,
    compatibility_note: Option<&str>,
) -> M3ComponentCatalogEntry {
    M3ComponentCatalogEntry {
        schema_version: M3_COMPONENT_CATALOG_SCHEMA_VERSION,
        source_id: "test-registry".to_string(),
        component_id: component_id.to_string(),
        kind: M3ComponentKind::LlamaCppServer,
        display_name: "llama.cpp server".to_string(),
        accelerator: Some(AcceleratorKind::Metal),
        version: version.to_string(),
        channel,
        download_url: "https://components.example.test/llama-server".to_string(),
        sha256: sha256(bytes),
        size_bytes: bytes.len() as u64,
        published_at_ms,
        compatibility_note: compatibility_note.map(|note| note.to_string()),
        metadata: BTreeMap::from([("target".to_string(), "aarch64-apple-darwin".to_string())]),
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

fn make_hub(root: &std::path::Path, download: Arc<dyn M3DownloadTransport>) -> M3ComponentHub {
    M3ComponentHub::new(
        root,
        test_config(),
        M3ComponentHubDependencies {
            clock: Arc::new(FixedClock::new(10_000)),
            download,
            sources: Vec::new(),
        },
    )
    .expect("M3 component hub")
}

#[tokio::test]
async fn install_resumes_verifies_activates_and_rolls_back() {
    let directory = TestDirectory::new("install");
    let first_bytes = payload(160_000, 7);
    let download = Arc::new(MutableDownload::new(first_bytes.clone(), "etag-v1"));
    download.fail_once_at(64 * 1024);
    let hub = make_hub(&directory.0, download.clone());
    let context = M3OperationContext::new(10_000);
    let first_entry = entry(
        &first_bytes,
        "llama-cpp-server-metal",
        "b4100",
        M3ComponentChannel::Stable,
        1_000,
        Some("requires macOS 14 or newer"),
    );

    // First attempt hits the injected one-shot failure after the first chunk.
    assert!(matches!(
        hub.install_component(
            &M3InstallComponentRequest {
                entry: first_entry.clone(),
            },
            &context,
        )
        .await,
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

    // Retrying resumes from the partial offset rather than restarting.
    let installed = hub
        .install_component(
            &M3InstallComponentRequest {
                entry: first_entry.clone(),
            },
            &context,
        )
        .await
        .expect("resume verified install");
    assert_eq!(installed.versions.len(), 1);
    assert_eq!(installed.component_id, "llama-cpp-server-metal");
    assert_eq!(installed.channel, M3ComponentChannel::Stable);
    assert_eq!(installed.accelerator, Some(AcceleratorKind::Metal));
    let active = installed.versions.iter().find(|v| v.active).unwrap();
    assert_eq!(active.version, "b4100");
    assert_eq!(
        active.compatibility_note.as_deref(),
        Some("requires macOS 14 or newer")
    );
    assert_eq!(fs::read(&active.artifact_path).unwrap(), first_bytes);
    assert!(download.offsets()[2..].starts_with(&[64 * 1024]));
    let first_version_key = installed.active_version_key.clone();

    // Repeating the exact same already-active install is a verified no-op:
    // no additional network reads happen beyond the initial probe/resume.
    let offsets_before = download.offsets().len();
    hub.install_component(
        &M3InstallComponentRequest {
            entry: first_entry.clone(),
        },
        &context,
    )
    .await
    .expect("idempotent reinstall of the active version");
    assert_eq!(download.offsets().len(), offsets_before);

    // Installing a new version activates it and keeps the prior version on
    // disk for rollback.
    let second_bytes = payload(175_000, 19);
    download.set_payload(second_bytes.clone(), "etag-v2");
    let second_entry = entry(
        &second_bytes,
        "llama-cpp-server-metal",
        "b4200",
        M3ComponentChannel::Stable,
        2_000,
        None,
    );
    let updated = hub
        .install_component(
            &M3InstallComponentRequest {
                entry: second_entry.clone(),
            },
            &context,
        )
        .await
        .expect("verified second install");
    assert_eq!(updated.versions.len(), 2);
    let active = updated.versions.iter().find(|v| v.active).unwrap();
    assert_eq!(active.version, "b4200");
    assert!(active.compatibility_note.is_none());
    assert_eq!(fs::read(&active.artifact_path).unwrap(), second_bytes);

    // Rollback: activate the previous, still-verified version.
    let rolled_back = hub
        .activate_component_version(
            &M3ActivateComponentVersionRequest {
                component_id: "llama-cpp-server-metal".to_string(),
                version_key: first_version_key.clone(),
            },
            &context,
        )
        .await
        .expect("activate the previous verified version");
    assert_eq!(rolled_back.active_version_key, first_version_key);
    assert_eq!(
        rolled_back
            .versions
            .iter()
            .find(|v| v.active)
            .unwrap()
            .version,
        "b4100"
    );

    // Activating an unknown version key is rejected.
    assert!(matches!(
        hub.activate_component_version(
            &M3ActivateComponentVersionRequest {
                component_id: "llama-cpp-server-metal".to_string(),
                version_key: "0".repeat(64),
            },
            &context,
        )
        .await,
        Err(M3HubError::NotFound(_))
    ));
}

#[tokio::test]
async fn install_rejects_a_digest_mismatch_before_it_is_ever_trusted() {
    let directory = TestDirectory::new("digest-mismatch");
    let bytes = payload(96_000, 31);
    let download = Arc::new(MutableDownload::new(bytes.clone(), "etag-corrupt"));
    let hub = make_hub(&directory.0, download);
    let context = M3OperationContext::new(10_000);

    let mut corrupt_entry = entry(
        &bytes,
        "tokenizer-bpe",
        "1.0.0",
        M3ComponentChannel::Beta,
        1_000,
        None,
    );
    corrupt_entry.sha256 = "f".repeat(64);

    assert!(matches!(
        hub.install_component(
            &M3InstallComponentRequest {
                entry: corrupt_entry,
            },
            &context,
        )
        .await,
        Err(M3HubError::Integrity { .. })
    ));
    assert!(hub.list_installed().unwrap().is_empty());
    // No partial or resume file survives an integrity rejection.
    let leftovers: Vec<_> = fs::read_dir(directory.0.join("downloads"))
        .expect("downloads")
        .filter_map(Result::ok)
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn bounded_retention_keeps_only_the_most_recent_versions() {
    let directory = TestDirectory::new("retention");
    let download = Arc::new(MutableDownload::new(payload(1_000, 0), "etag-0"));
    let hub = make_hub(&directory.0, download.clone());
    let context = M3OperationContext::new(10_000);

    // Install five distinct versions of the same component in sequence. The
    // hub must never keep more than `MAX_COMPONENT_VERSIONS_KEPT` (3) of
    // them, while always keeping the currently active one.
    let mut last_view = None;
    for index in 0..5u8 {
        let bytes = payload(1_000 + index as usize, index);
        download.set_payload(bytes.clone(), &format!("etag-{index}"));
        let version_entry = entry(
            &bytes,
            "cuda-support",
            &format!("12.{index}"),
            M3ComponentChannel::Stable,
            1_000 + index as u64,
            None,
        );
        last_view = Some(
            hub.install_component(
                &M3InstallComponentRequest {
                    entry: version_entry,
                },
                &context,
            )
            .await
            .expect("verified sequential install"),
        );
    }
    let view = last_view.expect("at least one install");
    assert_eq!(view.versions.len(), 3);
    assert!(view.versions.iter().any(|v| v.active && v.version == "12.4"));
    let kept_versions: Vec<&str> = view.versions.iter().map(|v| v.version.as_str()).collect();
    assert!(kept_versions.contains(&"12.4"));
    assert!(kept_versions.contains(&"12.3"));
    assert!(kept_versions.contains(&"12.2"));
    assert!(!kept_versions.contains(&"12.0"));
    assert!(!kept_versions.contains(&"12.1"));

    // Pruned version directories are actually removed from disk, not just
    // dropped from the state view.
    let asset_root = view.versions[0]
        .artifact_path
        .parent()
        .and_then(std::path::Path::parent)
        .expect("asset root")
        .to_path_buf();
    let on_disk_versions = fs::read_dir(&asset_root)
        .expect("asset root")
        .filter_map(Result::ok)
        .filter(|dirent| !dirent.file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(on_disk_versions, 3);
}

#[tokio::test]
async fn pinned_channel_never_reports_an_update_while_stable_does() {
    let directory = TestDirectory::new("channels");
    let stable_bytes = payload(2_000, 1);
    let download = Arc::new(MutableDownload::new(stable_bytes.clone(), "etag-stable"));
    let hub = make_hub(&directory.0, download.clone());
    let context = M3OperationContext::new(10_000);

    let installed_stable = entry(
        &stable_bytes,
        "mlx-runtime",
        "0.5.0",
        M3ComponentChannel::Stable,
        1_000,
        None,
    );
    hub.install_component(
        &M3InstallComponentRequest {
            entry: installed_stable.clone(),
        },
        &context,
    )
    .await
    .expect("install stable mlx-runtime");

    let pinned_bytes = payload(2_100, 2);
    download.set_payload(pinned_bytes.clone(), "etag-pinned");
    let installed_pinned = entry(
        &pinned_bytes,
        "vulkan-support",
        "1.0.0",
        M3ComponentChannel::Pinned,
        1_000,
        Some("known issue with pre-2020 AMD drivers"),
    );
    hub.install_component(
        &M3InstallComponentRequest {
            entry: installed_pinned.clone(),
        },
        &context,
    )
    .await
    .expect("install pinned vulkan-support");

    // A newer stable mlx-runtime and a newer *pinned* vulkan-support both
    // exist in the registry.
    let newer_stable_bytes = payload(2_200, 3);
    let newer_stable = entry(
        &newer_stable_bytes,
        "mlx-runtime",
        "0.6.0",
        M3ComponentChannel::Stable,
        2_000,
        None,
    );
    let newer_pinned_bytes = payload(2_300, 4);
    let newer_pinned = entry(
        &newer_pinned_bytes,
        "vulkan-support",
        "2.0.0",
        M3ComponentChannel::Pinned,
        2_000,
        None,
    );
    let registry = Arc::new(
        StaticM3ComponentSource::new(
            "test-registry",
            vec![
                installed_stable,
                newer_stable,
                installed_pinned,
                newer_pinned,
            ],
        )
        .expect("static component registry"),
    );
    hub.replace_sources(vec![registry]).expect("replace sources");

    let checks = hub.check_updates(&context).await.expect("check updates");
    let mlx_check = checks
        .iter()
        .find(|check| check.component_id == "mlx-runtime")
        .expect("mlx-runtime check");
    assert!(mlx_check.update_available);
    assert_eq!(
        mlx_check
            .latest_available
            .as_ref()
            .expect("latest stable entry")
            .version,
        "0.6.0"
    );

    let vulkan_check = checks
        .iter()
        .find(|check| check.component_id == "vulkan-support")
        .expect("vulkan-support check");
    assert!(!vulkan_check.update_available);
    assert!(vulkan_check.latest_available.is_none());
}

#[test]
fn component_catalog_entries_reject_blank_compatibility_notes() {
    let bytes = payload(100, 5);
    let mut bad_entry = entry(
        &bytes,
        "projector-clip",
        "1.0.0",
        M3ComponentChannel::Beta,
        1_000,
        None,
    );
    bad_entry.compatibility_note = Some("   ".to_string());
    assert!(matches!(
        bad_entry.validate(),
        Err(M3HubError::Invalid { .. })
    ));

    assert!(matches!(
        StaticM3ComponentSource::new("test-registry", vec![bad_entry]),
        Err(M3HubError::Invalid { .. })
    ));
}
