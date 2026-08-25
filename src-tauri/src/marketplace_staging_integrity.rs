//! Mutation-time integrity check for native marketplace staging leases.
//!
//! `marketplace_commands` proves that lease identity is still authorized by the
//! current signed M4 snapshot.  This module proves the materialized package
//! still represents the exact immutable `.lmx` identified by that lease.  Both
//! checks run before every preview/install/update handle use, closing the
//! prepare→preview/mutation TOCTOU window without giving the renderer any path,
//! URL, byte, or digest authority.

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKETPLACE_CACHE_DIR: &str = "extension-marketplace-cache-v2";
const MARKETPLACE_HANDLE_PREFIX: &str = "little-monkey-marketplace:v2:";
const MAX_LMX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;
const MAX_LMX_FILES: usize = 128;
const MAX_LMX_PATH_CHARS: usize = 512;
const MAX_LMX_FILE_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_DECODED_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = MAX_LMX_FILES * 4 + 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceLeaseIdentity {
    registry_source_id: String,
    registry_id: String,
    registry_snapshot_sha256: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
    created_unix_ms: u64,
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
        || normalized.as_bytes().get(1).is_some_and(|byte| *byte == b':')
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

fn id_from_handle(handle: &str) -> Result<Uuid, String> {
    let raw = handle
        .strip_prefix(MARKETPLACE_HANDLE_PREFIX)
        .ok_or_else(|| "Invalid marketplace staging handle".to_string())?;
    Uuid::parse_str(raw).map_err(|_| "Invalid marketplace staging handle".to_string())
}

fn lease_root(handle: &str) -> Result<PathBuf, String> {
    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())?;
    Ok(app_data
        .join(MARKETPLACE_CACHE_DIR)
        .join(id_from_handle(handle)?.to_string()))
}

fn read_lease(handle: &str) -> Result<(MarketplaceLeaseIdentity, PathBuf), String> {
    let root = lease_root(handle)?;
    let metadata = fs::symlink_metadata(&root)
        .map_err(|_| "Marketplace staging lease no longer exists".to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Marketplace staging lease is not a real directory".to_string());
    }
    let marker = root.join("lease.json");
    let marker_metadata = fs::symlink_metadata(&marker)
        .map_err(|_| "Marketplace lease metadata is missing".to_string())?;
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return Err("Marketplace lease metadata must be a real file".to_string());
    }
    let lease: MarketplaceLeaseIdentity = serde_json::from_slice(
        &fs::read(&marker).map_err(|error| format!("Cannot read marketplace lease: {error}"))?,
    )
    .map_err(|error| format!("Invalid marketplace lease metadata: {error}"))?;
    // Touch every identity field while decoding so future schema drift cannot
    // silently turn a security-relevant lease field into dead data here.
    if lease.registry_source_id.is_empty()
        || lease.registry_id.is_empty()
        || lease.registry_snapshot_sha256.is_empty()
        || lease.created_unix_ms == 0
    {
        return Err("Marketplace staging lease identity is incomplete".to_string());
    }
    Ok((lease, root.join("package")))
}

fn relative_ascii_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Marketplace staged file escaped its package root".to_string())?;
    let raw = relative.to_string_lossy().replace('\\', "/");
    let safe = safe_marketplace_path(&raw)?;
    Ok(safe.to_string_lossy().replace('\\', "/"))
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, String>,
    total_bytes: &mut usize,
    entries_seen: &mut usize,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("Cannot inspect marketplace staging directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Marketplace staged package contains an unsafe directory".to_string());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Cannot scan marketplace staged package: {error}"))?
    {
        *entries_seen = entries_seen.saturating_add(1);
        if *entries_seen > MAX_DIRECTORY_ENTRIES {
            return Err("Marketplace staged package exceeds its bounded directory-entry count".to_string());
        }
        let entry = entry.map_err(|error| format!("Cannot inspect marketplace staged entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Cannot inspect marketplace staged entry: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Marketplace staged package cannot contain symlinks".to_string());
        }
        if metadata.is_dir() {
            collect_files(root, &path, files, total_bytes, entries_seen)?;
            continue;
        }
        if !metadata.is_file() {
            return Err("Marketplace staged package contains an unsupported filesystem entry".to_string());
        }
        let relative = relative_ascii_path(root, &path)?;
        if relative == "extension.json" {
            continue;
        }
        if files.len() >= MAX_LMX_FILES {
            return Err("Marketplace staged package exceeds its bounded file count".to_string());
        }
        let length = usize::try_from(metadata.len())
            .map_err(|_| "Marketplace staged file length exceeds platform range".to_string())?;
        if length > MAX_LMX_FILE_BYTES {
            return Err(format!("Marketplace staged file exceeds its size limit: {relative}"));
        }
        *total_bytes = total_bytes
            .checked_add(length)
            .ok_or_else(|| "Marketplace staged package size overflow".to_string())?;
        if *total_bytes > MAX_LMX_DECODED_BYTES {
            return Err("Marketplace staged package exceeds its decoded size limit".to_string());
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("Cannot read marketplace staged file: {error}"))?;
        if files
            .insert(
                relative.clone(),
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )
            .is_some()
        {
            return Err(format!("Marketplace staged package contains duplicate path: {relative}"));
        }
    }
    Ok(())
}

/// Verify that the app-owned materialization still reproduces the exact signed
/// `.lmx` and manifest digests captured in the native lease.  The subsequent
/// marketplace command independently re-resolves those lease digests from the
/// *current* verified M4 state, so neither this file nor the renderer is an
/// authority for what may be installed.
pub fn validate_handle(handle: &str) -> Result<(), String> {
    let (lease, package) = read_lease(handle)?;
    let package_metadata = fs::symlink_metadata(&package)
        .map_err(|_| "Marketplace staged package is missing".to_string())?;
    if !package_metadata.is_dir() || package_metadata.file_type().is_symlink() {
        return Err("Marketplace staged package must be a real directory".to_string());
    }

    let manifest_path = package.join("extension.json");
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .map_err(|_| "Marketplace staged manifest is missing".to_string())?;
    if !manifest_metadata.is_file() || manifest_metadata.file_type().is_symlink() {
        return Err("Marketplace staged manifest must be a real file".to_string());
    }
    if manifest_metadata.len() > u64::try_from(MAX_LMX_DOWNLOAD_BYTES).unwrap_or(u64::MAX) {
        return Err("Marketplace staged manifest exceeds its bounded size".to_string());
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("Cannot read marketplace staged manifest: {error}"))?,
    )
    .map_err(|error| format!("Marketplace staged manifest is invalid JSON: {error}"))?;
    let manifest_canonical = canonical_json(&manifest)?;
    if manifest_canonical.len() > MAX_LMX_MANIFEST_BYTES
        || sha256_hex(manifest_canonical.as_bytes()) != lease.manifest_sha256.to_ascii_lowercase()
    {
        return Err("Marketplace staged manifest no longer matches the signed M4 identity".to_string());
    }
    if manifest.get("extension_id").and_then(serde_json::Value::as_str) != Some(lease.extension_id.as_str())
        || manifest.get("version").and_then(serde_json::Value::as_str) != Some(lease.version.as_str())
    {
        return Err("Marketplace staged manifest identity/version changed after preparation".to_string());
    }
    let registry_id = manifest
        .pointer("/provenance/source/curated_registry/registry_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Marketplace staged manifest lost curated-registry provenance".to_string())?;
    if registry_id != lease.registry_id {
        return Err("Marketplace staged manifest registry provenance changed after preparation".to_string());
    }

    let mut files_base64 = BTreeMap::new();
    let mut total_bytes = 0usize;
    let mut entries_seen = 0usize;
    collect_files(
        &package,
        &package,
        &mut files_base64,
        &mut total_bytes,
        &mut entries_seen,
    )?;
    if files_base64.is_empty() {
        return Err("Marketplace staged package has no executable payload files".to_string());
    }
    let envelope = serde_json::json!({
        "schema_version": 1,
        "manifest": manifest,
        "files_base64": files_base64,
    });
    let canonical = canonical_json(&envelope)?;
    if canonical.len() > MAX_LMX_DOWNLOAD_BYTES
        || sha256_hex(canonical.as_bytes()) != lease.package_sha256.to_ascii_lowercase()
    {
        return Err("Marketplace staged bytes no longer reproduce the signed M4 package digest".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_envelope_digest_changes_for_path_or_byte_mutation() {
        let base = serde_json::json!({
            "schema_version": 1,
            "manifest": {"extension_id":"com.example.echo","version":"1.0.0"},
            "files_base64": {"component.wasm":"AQID"},
        });
        let base_digest = sha256_hex(canonical_json(&base).unwrap().as_bytes());

        let byte_changed = serde_json::json!({
            "schema_version": 1,
            "manifest": {"extension_id":"com.example.echo","version":"1.0.0"},
            "files_base64": {"component.wasm":"AQIE"},
        });
        assert_ne!(sha256_hex(canonical_json(&byte_changed).unwrap().as_bytes()), base_digest);

        let path_changed = serde_json::json!({
            "schema_version": 1,
            "manifest": {"extension_id":"com.example.echo","version":"1.0.0"},
            "files_base64": {"renamed.wasm":"AQID"},
        });
        assert_ne!(sha256_hex(canonical_json(&path_changed).unwrap().as_bytes()), base_digest);
    }

    #[test]
    fn unsafe_relative_paths_are_rejected() {
        assert!(safe_marketplace_path("../escape").is_err());
        assert!(safe_marketplace_path("/absolute").is_err());
        assert!(safe_marketplace_path("C:/drive").is_err());
        assert!(safe_marketplace_path("safe/component.wasm").is_ok());
    }
}
