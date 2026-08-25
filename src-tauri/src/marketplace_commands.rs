//! Native executable-extension marketplace boundary.
//!
//! The renderer submits only a verified registry source id, the snapshot SHA it
//! reviewed, and an extension id/version. Rust resolves immutable package and
//! manifest digests from the currently verified M4 state, derives and fetches
//! the artifact itself through the hardened executable-extension HTTP client,
//! stages it under app-owned storage, and returns an opaque lease. Every
//! preview/install/update re-resolves that lease against current M4 state so
//! expiry, revocation, source removal, or snapshot replacement fails closed.

use crate::executable_extensions::{Approval, ExtensionDetail, ExtensionManager, ExtensionPreview};
use crate::m4_commands::M4CommandState;
use crate::package_ecosystem::{
    signed_first_party_catalog, AdditionalRegistryRecord, RegistrySnapshot, RevocationTarget,
    SemanticVersion,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;
use uuid::Uuid;

const MARKETPLACE_CACHE_DIR: &str = "extension-marketplace-cache-v2";
const MARKETPLACE_HANDLE_PREFIX: &str = "little-monkey-marketplace:v2:";
const MAX_REGISTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_LMX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;
const MAX_LMX_FILES: usize = 128;
const MAX_LMX_PATH_CHARS: usize = 512;
const MAX_LMX_FILE_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_DECODED_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_MANIFEST_BYTES: usize = 256 * 1024;
const STAGING_TTL_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplacePrepareRequest {
    pub registry_source_id: String,
    pub registry_snapshot_sha256: String,
    pub extension_id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
struct ResolvedMarketplaceEntry {
    registry_source_id: String,
    registry_id: String,
    registry_snapshot_sha256: String,
    registry_location: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceLease {
    registry_source_id: String,
    registry_id: String,
    registry_snapshot_sha256: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
    created_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceEnvelope {
    schema_version: u32,
    manifest: serde_json::Value,
    files_base64: BTreeMap<String, String>,
}

fn manager() -> Result<ExtensionManager, String> {
    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())?;
    ExtensionManager::new(app_data)
}

fn now_unix_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before Unix epoch: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "System clock exceeds marketplace timestamp range".to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn canonical_json(value: &serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => serde_json::to_string(value)
            .map_err(|error| format!("Cannot encode canonical marketplace JSON: {error}")),
        serde_json::Value::Array(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        )),
        serde_json::Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut fields = Vec::with_capacity(keys.len());
            for key in keys {
                fields.push(format!(
                    "{}:{}",
                    serde_json::to_string(key)
                        .map_err(|error| format!("Cannot encode marketplace JSON key: {error}"))?,
                    canonical_json(&object[key])?
                ));
            }
            Ok(format!("{{{}}}", fields.join(",")))
        }
    }
}

fn safe_marketplace_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.len() > MAX_LMX_PATH_CHARS || raw.contains('\0') {
        return Err(format!("Unsafe marketplace package path: {raw}"));
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':')
    {
        return Err(format!("Unsafe marketplace package path: {raw}"));
    }
    let mut path = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." || part == ".." || !part.is_ascii() {
            return Err(format!("Unsafe marketplace package path: {raw}"));
        }
        path.push(part);
    }
    Ok(path)
}

fn claim_marketplace_package_path(collisions: &mut BTreeSet<String>, relative: &Path) -> bool {
    let key = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    key != "extension.json" && collisions.insert(key)
}

fn require_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Marketplace mutations are allowed only from the main window".to_string())
    }
}

fn package_revoked(
    snapshot: &RegistrySnapshot,
    package_id: &str,
    version: SemanticVersion,
    now: u64,
) -> Option<String> {
    snapshot.revocations.iter().find_map(|entry| {
        if entry.effective_unix_ms > now {
            return None;
        }
        let matches = match &entry.target {
            RevocationTarget::Package { package_id: target } => target == package_id,
            RevocationTarget::PackageVersion {
                package_id: target,
                version: target_version,
            } => target == package_id && *target_version == version,
            _ => false,
        };
        matches.then(|| entry.reason.clone())
    })
}

fn active_release_identity(
    snapshot: &RegistrySnapshot,
    package_id: &str,
    version: SemanticVersion,
    now: u64,
) -> Option<(String, String)> {
    if now >= snapshot.expires_unix_ms || package_revoked(snapshot, package_id, version, now).is_some() {
        return None;
    }
    snapshot
        .packages
        .get(package_id)?
        .iter()
        .find(|release| release.version == version)
        .map(|release| {
            (
                release.bundle_sha256.to_ascii_lowercase(),
                release.manifest_sha256.to_ascii_lowercase(),
            )
        })
}

fn require_consistent_registry_identity(
    records: &[AdditionalRegistryRecord],
    package_id: &str,
    version: SemanticVersion,
    expected_bundle_sha256: &str,
    expected_manifest_sha256: &str,
    now: u64,
) -> Result<(), String> {
    let expected_bundle_sha256 = expected_bundle_sha256.to_ascii_lowercase();
    let expected_manifest_sha256 = expected_manifest_sha256.to_ascii_lowercase();

    for record in records {
        let Some(verified) = record.verified.as_ref() else {
            continue;
        };
        let Some((bundle_sha256, manifest_sha256)) =
            active_release_identity(verified.snapshot(), package_id, version, now)
        else {
            continue;
        };
        if bundle_sha256 != expected_bundle_sha256 || manifest_sha256 != expected_manifest_sha256 {
            return Err(format!(
                "Verified M4 registries disagree on immutable digests for {package_id}@{version}; refresh/review the registry conflict before installing"
            ));
        }
    }

    // The bundled first-party registry has no remote executable artifacts today,
    // but it is still part of the M4 trust namespace. If a future bundled
    // snapshot indexes the same extension/version, a conflicting remote source
    // must fail closed rather than bypassing first-party provenance.
    let (_, first_party_snapshot, _) = signed_first_party_catalog().map_err(|error| error.to_string())?;
    if let Some((bundle_sha256, manifest_sha256)) =
        active_release_identity(&first_party_snapshot, package_id, version, now)
    {
        if bundle_sha256 != expected_bundle_sha256 || manifest_sha256 != expected_manifest_sha256 {
            return Err(format!(
                "Built-in first-party M4 catalog conflicts with the selected registry for {package_id}@{version}"
            ));
        }
    }
    Ok(())
}

fn resolve_marketplace_entry(
    state: &M4CommandState,
    request: &MarketplacePrepareRequest,
    now: u64,
) -> Result<ResolvedMarketplaceEntry, String> {
    let records = state
        .packages
        .list_registry_sources()
        .map_err(|error| error.to_string())?;
    let record = records
        .iter()
        .find(|record| record.source.source_id == request.registry_source_id)
        .ok_or_else(|| "Marketplace registry source is no longer configured".to_string())?;
    let verified = record
        .verified
        .as_ref()
        .ok_or_else(|| "Marketplace registry has no verified M4 snapshot".to_string())?;
    if verified.snapshot_sha256() != request.registry_snapshot_sha256 {
        return Err("Marketplace registry snapshot changed; refresh and review again".to_string());
    }
    let snapshot = verified.snapshot();
    if now >= snapshot.expires_unix_ms {
        return Err("Marketplace registry snapshot has expired".to_string());
    }
    let version = SemanticVersion::parse(&request.version).map_err(|error| error.to_string())?;
    let package_id = format!("extension.{}", request.extension_id);
    let catalog = snapshot
        .packages
        .get(&package_id)
        .ok_or_else(|| "Extension is not present in the verified M4 catalog".to_string())?;
    let release = catalog
        .iter()
        .find(|release| release.version == version)
        .ok_or_else(|| "Extension version is not present in the verified M4 catalog".to_string())?;
    if let Some(reason) = package_revoked(snapshot, &package_id, version, now) {
        return Err(format!("Marketplace release is revoked: {reason}"));
    }
    let package_sha256 = release.bundle_sha256.clone();
    let manifest_sha256 = release.manifest_sha256.clone();
    require_consistent_registry_identity(
        &records,
        &package_id,
        version,
        &package_sha256,
        &manifest_sha256,
        now,
    )?;
    Ok(ResolvedMarketplaceEntry {
        registry_source_id: record.source.source_id.clone(),
        registry_id: snapshot.registry_id.clone(),
        registry_snapshot_sha256: verified.snapshot_sha256().to_string(),
        registry_location: record.source.location.clone(),
        extension_id: request.extension_id.clone(),
        version: request.version.clone(),
        package_sha256,
        manifest_sha256,
    })
}

fn marketplace_artifact_url(entry: &ResolvedMarketplaceEntry) -> Result<Url, String> {
    let mut url = Url::parse(&entry.registry_location)
        .map_err(|error| format!("Invalid marketplace registry URL: {error}"))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err("Executable marketplace registries must use credential-free HTTPS".to_string());
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().to_string();
    let directory = path
        .rfind('/')
        .map(|index| &path[..=index])
        .unwrap_or("/");
    url.set_path(directory);
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "Marketplace registry URL cannot be used as a hierarchical base".to_string())?;
        segments
            .push("extensions")
            .push(&entry.extension_id)
            .push(&format!("{}.lmx", entry.version));
    }
    Ok(url)
}

async fn fetch_text(url: &Url, max_bytes: usize) -> Result<String, String> {
    crate::web::validate_fetch_url(url, false)
        .map_err(|denial| format!("Marketplace network target refused: {denial}"))?;
    let client = crate::web::executable_extension_http_client(Duration::from_secs(30))
        .map_err(|error| format!("Cannot build marketplace HTTP client: {error}"))?;
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| format!("Marketplace fetch failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Marketplace fetch returned HTTP {}", response.status()));
    }
    if response.url().origin() != url.origin() {
        return Err("Marketplace fetch escaped the verified registry origin".to_string());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if content_type != "application/json"
        && !content_type.ends_with("+json")
        && content_type != "text/plain"
    {
        return Err(format!(
            "Marketplace content must be JSON/text, received {}",
            if content_type.is_empty() { "unknown content type" } else { &content_type }
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Cannot read marketplace response: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err("Marketplace response exceeds its bounded size".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| "Marketplace response is not UTF-8 JSON/text".to_string())
}

fn marketplace_cache_root() -> Result<PathBuf, String> {
    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())?;
    let root = app_data.join(MARKETPLACE_CACHE_DIR);
    if root.exists() && fs::symlink_metadata(&root).map_err(|e| e.to_string())?.file_type().is_symlink() {
        return Err("Marketplace cache root cannot be a symlink".to_string());
    }
    fs::create_dir_all(&root).map_err(|error| format!("Cannot create marketplace cache: {error}"))?;
    Ok(root)
}

fn handle_for(id: Uuid) -> String {
    format!("{MARKETPLACE_HANDLE_PREFIX}{id}")
}

fn id_from_handle(handle: &str) -> Result<Uuid, String> {
    let raw = handle
        .strip_prefix(MARKETPLACE_HANDLE_PREFIX)
        .ok_or_else(|| "Invalid marketplace staging handle".to_string())?;
    Uuid::parse_str(raw).map_err(|_| "Invalid marketplace staging handle".to_string())
}

fn lease_root(handle: &str) -> Result<PathBuf, String> {
    Ok(marketplace_cache_root()?.join(id_from_handle(handle)?.to_string()))
}

fn cleanup_handle(handle: &str) -> Result<bool, String> {
    let root = lease_root(handle)?;
    if !root.exists() {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(&root).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Marketplace staging lease is not a real directory".to_string());
    }
    fs::remove_dir_all(root).map_err(|error| format!("Cannot remove marketplace staging lease: {error}"))?;
    Ok(true)
}

fn lease_is_expired(created_unix_ms: u64, now: u64) -> bool {
    now.saturating_sub(created_unix_ms) > STAGING_TTL_MS
}

fn prune_stale_leases(now: u64) -> Result<(), String> {
    let root = marketplace_cache_root()?;
    for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let marker = path.join("lease.json");
        let Ok(raw) = fs::read_to_string(&marker) else { continue };
        let Ok(lease) = serde_json::from_str::<MarketplaceLease>(&raw) else { continue };
        if lease_is_expired(lease.created_unix_ms, now) {
            let _ = fs::remove_dir_all(path);
        }
    }
    Ok(())
}

fn validate_marketplace_provenance(
    manifest: &serde_json::Value,
    registry_id: &str,
) -> Result<(), String> {
    let declared = manifest
        .pointer("/provenance/source/curated_registry/registry_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Marketplace extension provenance must name its curated M4 registry".to_string())?;
    if declared != registry_id {
        return Err("Marketplace extension provenance registry differs from the verified M4 registry".to_string());
    }
    Ok(())
}

fn stage_package(
    raw: &str,
    entry: &ResolvedMarketplaceEntry,
    now: u64,
) -> Result<(String, PathBuf), String> {
    if raw.is_empty() || raw.len() > MAX_LMX_DOWNLOAD_BYTES {
        return Err("Marketplace .lmx exceeds its bounded encoded size".to_string());
    }
    if sha256_hex(raw.as_bytes()) != entry.package_sha256.to_ascii_lowercase() {
        return Err("Marketplace .lmx does not match the native-resolved M4 package digest".to_string());
    }
    let envelope: MarketplaceEnvelope = serde_json::from_str(raw)
        .map_err(|error| format!("Marketplace .lmx is not valid JSON: {error}"))?;
    if envelope.schema_version != 1 || envelope.files_base64.is_empty() || envelope.files_base64.len() > MAX_LMX_FILES {
        return Err("Marketplace .lmx has an invalid schema or file count".to_string());
    }
    let manifest_canonical = canonical_json(&envelope.manifest)?;
    if manifest_canonical.len() > MAX_LMX_MANIFEST_BYTES
        || sha256_hex(manifest_canonical.as_bytes()) != entry.manifest_sha256.to_ascii_lowercase()
    {
        return Err("Marketplace manifest does not match native-resolved signed M4 metadata".to_string());
    }
    let manifest_id = envelope
        .manifest
        .get("extension_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Marketplace manifest is missing extension_id".to_string())?;
    let manifest_version = envelope
        .manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Marketplace manifest is missing version".to_string())?;
    if manifest_id != entry.extension_id || manifest_version != entry.version {
        return Err("Marketplace manifest identity/version disagrees with native M4 metadata".to_string());
    }
    validate_marketplace_provenance(&envelope.manifest, &entry.registry_id)?;
    let component = safe_marketplace_path(
        envelope
            .manifest
            .pointer("/component/path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Marketplace manifest is missing component.path".to_string())?,
    )?;

    let mut decoded = Vec::with_capacity(envelope.files_base64.len());
    let mut total = 0usize;
    let mut collisions = BTreeSet::new();
    for (raw_path, encoded) in envelope.files_base64 {
        let relative = safe_marketplace_path(&raw_path)?;
        if !claim_marketplace_package_path(&mut collisions, &relative) {
            return Err(format!("Marketplace .lmx contains a duplicate/reserved path: {raw_path}"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| format!("Marketplace .lmx contains invalid base64 for {raw_path}"))?;
        if bytes.len() > MAX_LMX_FILE_BYTES {
            return Err(format!("Marketplace file exceeds its size limit: {raw_path}"));
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| "Marketplace decoded size overflow".to_string())?;
        if total > MAX_LMX_DECODED_BYTES {
            return Err("Marketplace .lmx decoded payload exceeds its limit".to_string());
        }
        decoded.push((relative, bytes));
    }
    if !decoded.iter().any(|(path, _)| path == &component) {
        return Err("Marketplace .lmx is missing its declared component".to_string());
    }

    prune_stale_leases(now)?;
    let id = Uuid::new_v4();
    let root = marketplace_cache_root()?.join(id.to_string());
    fs::create_dir(&root).map_err(|error| format!("Cannot create marketplace lease: {error}"))?;
    let package = root.join("package");
    fs::create_dir(&package).map_err(|error| format!("Cannot create marketplace package directory: {error}"))?;
    let lease = MarketplaceLease {
        registry_source_id: entry.registry_source_id.clone(),
        registry_id: entry.registry_id.clone(),
        registry_snapshot_sha256: entry.registry_snapshot_sha256.clone(),
        extension_id: entry.extension_id.clone(),
        version: entry.version.clone(),
        package_sha256: entry.package_sha256.clone(),
        manifest_sha256: entry.manifest_sha256.clone(),
        created_unix_ms: now,
    };
    let write_result = (|| -> Result<(), String> {
        fs::write(
            root.join("lease.json"),
            serde_json::to_vec_pretty(&lease).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("Cannot write marketplace lease: {error}"))?;
        fs::write(package.join("extension.json"), format!("{}\n", serde_json::to_string_pretty(&envelope.manifest).map_err(|error| error.to_string())?))
            .map_err(|error| format!("Cannot stage marketplace manifest: {error}"))?;
        for (relative, bytes) in decoded {
            let target = package.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| format!("Cannot create marketplace package directory: {error}"))?;
            }
            fs::write(target, bytes).map_err(|error| format!("Cannot stage marketplace file: {error}"))?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&root);
        return Err(error);
    }
    Ok((handle_for(id), package))
}

fn load_lease(handle: &str) -> Result<(MarketplaceLease, PathBuf), String> {
    let root = lease_root(handle)?;
    let metadata = fs::symlink_metadata(&root)
        .map_err(|_| "Marketplace staging lease no longer exists".to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Marketplace staging lease is not a real directory".to_string());
    }
    let marker = root.join("lease.json");
    if fs::symlink_metadata(&marker).map_err(|_| "Marketplace lease metadata is missing".to_string())?.file_type().is_symlink() {
        return Err("Marketplace lease metadata cannot be a symlink".to_string());
    }
    let lease: MarketplaceLease = serde_json::from_slice(
        &fs::read(&marker).map_err(|error| format!("Cannot read marketplace lease: {error}"))?,
    )
    .map_err(|error| format!("Invalid marketplace lease metadata: {error}"))?;
    Ok((lease, root.join("package")))
}

fn resolve_handle(
    state: &M4CommandState,
    handle: &str,
    now: u64,
) -> Result<PathBuf, String> {
    let (lease, package) = load_lease(handle)?;
    if lease_is_expired(lease.created_unix_ms, now) {
        let _ = cleanup_handle(handle);
        return Err("Marketplace staging lease has expired; prepare and review the release again".to_string());
    }
    let request = MarketplacePrepareRequest {
        registry_source_id: lease.registry_source_id.clone(),
        registry_snapshot_sha256: lease.registry_snapshot_sha256.clone(),
        extension_id: lease.extension_id.clone(),
        version: lease.version.clone(),
    };
    let current = resolve_marketplace_entry(state, &request, now)?;
    if current.registry_id != lease.registry_id
        || current.package_sha256 != lease.package_sha256
        || current.manifest_sha256 != lease.manifest_sha256
    {
        return Err("Marketplace signed identity changed after preview; review again".to_string());
    }
    if !package.exists() || fs::symlink_metadata(&package).map_err(|e| e.to_string())?.file_type().is_symlink() {
        return Err("Marketplace staged package is missing or unsafe".to_string());
    }
    Ok(package)
}

fn preview_with_handle(mut preview: ExtensionPreview, handle: String) -> ExtensionPreview {
    preview.source_path = handle;
    preview
}

pub async fn marketplace_refresh_registries(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<AdditionalRegistryRecord>, String> {
    require_main_window(&window)?;
    let now = now_unix_ms()?;
    let records = state.packages.list_registry_sources().map_err(|error| error.to_string())?;
    let mut output = Vec::with_capacity(records.len());
    for record in records {
        let refreshed = async {
            let url = Url::parse(&record.source.location)
                .map_err(|error| format!("Invalid registry URL: {error}"))?;
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Err("Executable marketplace registries must use credential-free HTTPS".to_string());
            }
            let raw = fetch_text(&url, MAX_REGISTRY_BYTES).await?;
            let snapshot: RegistrySnapshot = serde_json::from_str(&raw)
                .map_err(|error| format!("Registry snapshot is not valid M4 JSON: {error}"))?;
            let fetched_sha = sha256_hex(&serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?);
            if record
                .verified
                .as_ref()
                .is_some_and(|verified| verified.snapshot_sha256() == fetched_sha)
            {
                return Ok(record.clone());
            }
            state
                .packages
                .verify_registry_source(&record.source.source_id, snapshot, now)
                .map_err(|error| error.to_string())
        }
        .await;
        match refreshed {
            Ok(record) => output.push(record),
            Err(error) => {
                let mut retained = record;
                retained.last_verification_error = Some(error);
                output.push(retained);
            }
        }
    }
    Ok(output)
}

pub async fn marketplace_prepare_extension(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    request: MarketplacePrepareRequest,
) -> Result<ExtensionPreview, String> {
    require_main_window(&window)?;
    let now = now_unix_ms()?;
    let entry = resolve_marketplace_entry(&state, &request, now)?;
    let artifact = marketplace_artifact_url(&entry)?;
    let raw = fetch_text(&artifact, MAX_LMX_DOWNLOAD_BYTES).await?;
    let (handle, package) = stage_package(&raw, &entry, now)?;
    match manager()?.discover(package.to_string_lossy().to_string()) {
        Ok(preview) => Ok(preview_with_handle(preview, handle)),
        Err(error) => {
            let _ = cleanup_handle(&handle);
            Err(error)
        }
    }
}

pub async fn marketplace_preview_install(
    state: tauri::State<'_, M4CommandState>,
    staging_handle: String,
) -> Result<ExtensionPreview, String> {
    let package = resolve_handle(&state, &staging_handle, now_unix_ms()?)?;
    manager()?
        .discover(package.to_string_lossy().to_string())
        .map(|preview| preview_with_handle(preview, staging_handle))
}

pub async fn marketplace_preview_update(
    state: tauri::State<'_, M4CommandState>,
    staging_handle: String,
) -> Result<ExtensionPreview, String> {
    let package = resolve_handle(&state, &staging_handle, now_unix_ms()?)?;
    manager()?
        .preview_update(package.to_string_lossy().to_string())
        .map(|preview| preview_with_handle(preview, staging_handle))
}

pub async fn marketplace_install_extension(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    staging_handle: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    require_main_window(&window)?;
    let package = resolve_handle(&state, &staging_handle, now_unix_ms()?)?;
    let result = manager()?
        .install(package.to_string_lossy().to_string(), approval)
        .await;
    let cleanup = cleanup_handle(&staging_handle);
    match (result, cleanup) {
        (Ok(detail), Ok(_)) => Ok(detail),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub async fn marketplace_update_extension(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    staging_handle: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    require_main_window(&window)?;
    let package = resolve_handle(&state, &staging_handle, now_unix_ms()?)?;
    let result = manager()?
        .update(package.to_string_lossy().to_string(), approval)
        .await;
    let cleanup = cleanup_handle(&staging_handle);
    match (result, cleanup) {
        (Ok(detail), Ok(_)) => Ok(detail),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub fn marketplace_cleanup_extension(staging_handle: String) -> Result<bool, String> {
    cleanup_handle(&staging_handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_cannot_smuggle_raw_bytes_or_hashes_into_prepare_request() {
        let raw = r#"{
            "registry_source_id":"team",
            "registry_snapshot_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "extension_id":"com.example.echo",
            "version":"1.2.3",
            "raw_package":"self-authorized",
            "package_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }"#;
        assert!(serde_json::from_str::<MarketplacePrepareRequest>(raw).is_err());
    }

    #[test]
    fn opaque_handle_rejects_paths_and_non_uuids() {
        assert!(id_from_handle("/tmp/package").is_err());
        assert!(id_from_handle("little-monkey-marketplace:v2:../escape").is_err());
        let id = Uuid::new_v4();
        assert_eq!(id_from_handle(&handle_for(id)).unwrap(), id);
    }

    #[test]
    fn staging_lease_ttl_is_enforced_at_use_boundary() {
        let now = STAGING_TTL_MS + 10;
        assert!(!lease_is_expired(now - STAGING_TTL_MS, now));
        assert!(lease_is_expired(now - STAGING_TTL_MS - 1, now));
        assert!(!lease_is_expired(now + 1, now));
    }

    #[test]
    fn package_path_collisions_and_manifest_aliases_fail_closed() {
        let mut claimed = BTreeSet::new();
        assert!(!claim_marketplace_package_path(
            &mut claimed,
            Path::new("Extension.json")
        ));
        assert!(claim_marketplace_package_path(
            &mut claimed,
            Path::new("component.wasm")
        ));
        assert!(!claim_marketplace_package_path(
            &mut claimed,
            Path::new("Component.wasm")
        ));
    }

    #[test]
    fn bundled_catalog_is_part_of_marketplace_identity_namespace() {
        let (_, snapshot, _) = signed_first_party_catalog().expect("first-party catalog");
        for (package_id, releases) in &snapshot.packages {
            if !package_id.starts_with("extension.") {
                continue;
            }
            for release in releases {
                let identity = active_release_identity(
                    &snapshot,
                    package_id,
                    release.version,
                    snapshot.generated_unix_ms,
                )
                .expect("active first-party extension identity");
                assert_eq!(identity.0, release.bundle_sha256.to_ascii_lowercase());
                assert_eq!(identity.1, release.manifest_sha256.to_ascii_lowercase());
            }
        }
    }
}
