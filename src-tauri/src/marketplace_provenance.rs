//! Durable provenance journal for executable marketplace mutations.
//!
//! Marketplace staging leases are intentionally short-lived.  This module
//! persists the immutable signed-registry identity that authorized a successful
//! install/update independently of that staging directory.  A receipt is first
//! journaled as `authorized` before mutation, then promoted to `committed` only
//! after the runtime result matches the exact extension/version/manifest and
//! curated-registry identity.  If the process dies between those two writes,
//! normal extension listing reconciles the journal against installed runtime
//! state.  The renderer never supplies receipt fields.

use crate::executable_extensions::ExtensionDetail;
use crate::package_ecosystem::InstallSource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const MARKETPLACE_CACHE_DIR: &str = "extension-marketplace-cache-v2";
const MARKETPLACE_PROVENANCE_DIR: &str = "extension-marketplace-provenance-v1";
const MARKETPLACE_HANDLE_PREFIX: &str = "little-monkey-marketplace:v2:";
const AUTHORIZED_RECEIPT_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketplaceMutationKind {
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReceiptState {
    Authorized,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceProvenanceReceipt {
    schema_version: u32,
    origin: String,
    state: ReceiptState,
    mutation: MarketplaceMutationKind,
    registry_source_id: String,
    registry_id: String,
    registry_snapshot_sha256: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
    lease_created_unix_ms: u64,
    authorized_unix_ms: u64,
    committed_unix_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AuthorizedMarketplaceReceipt {
    path: PathBuf,
    receipt: MarketplaceProvenanceReceipt,
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

fn app_data_dir() -> Result<PathBuf, String> {
    crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("Cannot inspect marketplace provenance directory: {error}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Marketplace provenance storage must be a real directory".to_string());
        }
        return Ok(());
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("Cannot create marketplace provenance directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect marketplace provenance directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Marketplace provenance storage must be a real directory".to_string());
    }
    Ok(())
}

fn provenance_root() -> Result<PathBuf, String> {
    let root = app_data_dir()?.join(MARKETPLACE_PROVENANCE_DIR);
    ensure_real_directory(&root)?;
    Ok(root)
}

fn id_from_handle(handle: &str) -> Result<Uuid, String> {
    let raw = handle
        .strip_prefix(MARKETPLACE_HANDLE_PREFIX)
        .ok_or_else(|| "Invalid marketplace staging handle".to_string())?;
    Uuid::parse_str(raw).map_err(|_| "Invalid marketplace staging handle".to_string())
}

fn read_lease_identity(handle: &str) -> Result<MarketplaceLeaseIdentity, String> {
    let lease_id = id_from_handle(handle)?;
    let cache_root = app_data_dir()?.join(MARKETPLACE_CACHE_DIR);
    let lease_root = cache_root.join(lease_id.to_string());
    let root_metadata = fs::symlink_metadata(&lease_root)
        .map_err(|_| "Marketplace staging lease no longer exists".to_string())?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err("Marketplace staging lease is not a real directory".to_string());
    }
    let marker = lease_root.join("lease.json");
    let marker_metadata = fs::symlink_metadata(&marker)
        .map_err(|_| "Marketplace lease metadata is missing".to_string())?;
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return Err("Marketplace lease metadata must be a real file".to_string());
    }
    serde_json::from_slice(
        &fs::read(&marker).map_err(|error| format!("Cannot read marketplace lease: {error}"))?,
    )
    .map_err(|error| format!("Invalid marketplace lease metadata: {error}"))
}

fn receipt_identity_key(receipt: &MarketplaceProvenanceReceipt) -> String {
    sha256_hex(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            receipt.registry_source_id,
            receipt.registry_snapshot_sha256,
            receipt.extension_id,
            receipt.version,
            receipt.package_sha256,
            receipt.manifest_sha256,
        )
        .as_bytes(),
    )
}

fn receipt_path(receipt: &MarketplaceProvenanceReceipt) -> Result<PathBuf, String> {
    Ok(provenance_root()?.join(format!("{}.json", receipt_identity_key(receipt))))
}

fn atomic_write_json(path: &Path, receipt: &MarketplaceProvenanceReceipt) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Marketplace provenance receipt has no parent directory".to_string())?;
    ensure_real_directory(parent)?;
    let temp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|error| format!("Cannot encode marketplace provenance receipt: {error}"))?;
    let result = (|| -> Result<(), String> {
        fs::write(&temp, bytes)
            .map_err(|error| format!("Cannot write marketplace provenance receipt: {error}"))?;
        fs::rename(&temp, path)
            .map_err(|error| format!("Cannot commit marketplace provenance receipt: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn detail_registry_id(detail: &ExtensionDetail) -> Result<&str, String> {
    match &detail.manifest.provenance.source {
        InstallSource::CuratedRegistry { registry_id } => Ok(registry_id.as_str()),
        _ => Err("Marketplace runtime result lost its curated-registry manifest provenance".to_string()),
    }
}

fn validate_result_identity(
    receipt: &MarketplaceProvenanceReceipt,
    extension_id: &str,
    version: &str,
    manifest_sha256: &str,
    registry_id: &str,
) -> Result<(), String> {
    if extension_id != receipt.extension_id {
        return Err("Marketplace runtime result extension id differs from its signed receipt".to_string());
    }
    if version != receipt.version {
        return Err("Marketplace runtime result version differs from its signed receipt".to_string());
    }
    if manifest_sha256.to_ascii_lowercase() != receipt.manifest_sha256.to_ascii_lowercase() {
        return Err("Marketplace runtime result manifest digest differs from its signed receipt".to_string());
    }
    if registry_id != receipt.registry_id {
        return Err("Marketplace runtime result registry provenance differs from its signed receipt".to_string());
    }
    Ok(())
}

fn receipt_matches_detail(receipt: &MarketplaceProvenanceReceipt, detail: &ExtensionDetail) -> bool {
    detail_registry_id(detail)
        .and_then(|registry_id| {
            validate_result_identity(
                receipt,
                &detail.manifest.extension_id,
                &detail.active_version,
                &detail.trust.manifest_sha256,
                registry_id,
            )
        })
        .is_ok()
}

/// Persist immutable registry authorization before the runtime mutation starts.
/// If the process crashes after runtime success, reconciliation can still prove
/// which signed snapshot/package/manifest identity authorized the mutation.
pub fn authorize_from_handle(
    handle: &str,
    mutation: MarketplaceMutationKind,
) -> Result<AuthorizedMarketplaceReceipt, String> {
    let lease = read_lease_identity(handle)?;
    let now = now_unix_ms()?;
    let receipt = MarketplaceProvenanceReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        origin: "marketplace".to_string(),
        state: ReceiptState::Authorized,
        mutation,
        registry_source_id: lease.registry_source_id,
        registry_id: lease.registry_id,
        registry_snapshot_sha256: lease.registry_snapshot_sha256,
        extension_id: lease.extension_id,
        version: lease.version,
        package_sha256: lease.package_sha256,
        manifest_sha256: lease.manifest_sha256,
        lease_created_unix_ms: lease.created_unix_ms,
        authorized_unix_ms: now,
        committed_unix_ms: None,
    };
    let path = receipt_path(&receipt)?;
    atomic_write_json(&path, &receipt)?;
    Ok(AuthorizedMarketplaceReceipt { path, receipt })
}

/// Bind the journaled M4 identity to the exact successful runtime result.  The
/// existing `installed_source` field remains the runtime's transport observation;
/// this receipt is the authoritative durable marketplace distribution provenance.
pub fn commit(
    authorized: &AuthorizedMarketplaceReceipt,
    detail: &ExtensionDetail,
) -> Result<(), String> {
    let registry_id = detail_registry_id(detail)?;
    validate_result_identity(
        &authorized.receipt,
        &detail.manifest.extension_id,
        &detail.active_version,
        &detail.trust.manifest_sha256,
        registry_id,
    )?;
    let mut committed = authorized.receipt.clone();
    committed.state = ReceiptState::Committed;
    committed.committed_unix_ms = Some(now_unix_ms()?);
    atomic_write_json(&authorized.path, &committed)
}

/// Reconcile crash-interrupted `authorized` receipts against installed runtime
/// state.  Uncommitted authorization records that never became installed are
/// removed after a bounded period; committed audit receipts are never pruned.
pub fn reconcile(installed: &[ExtensionDetail]) -> Result<(), String> {
    let root = provenance_root()?;
    let now = now_unix_ms()?;
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("Cannot scan marketplace provenance receipts: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Cannot inspect marketplace provenance receipt: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Cannot inspect marketplace provenance receipt: {error}"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let mut receipt: MarketplaceProvenanceReceipt =
            match serde_json::from_slice::<MarketplaceProvenanceReceipt>(&raw) {
                Ok(receipt)
                    if receipt.schema_version == RECEIPT_SCHEMA_VERSION
                        && receipt.origin == "marketplace" =>
                {
                    receipt
                }
                _ => continue,
            };
        if receipt.state == ReceiptState::Committed {
            continue;
        }
        if installed.iter().any(|detail| receipt_matches_detail(&receipt, detail)) {
            receipt.state = ReceiptState::Committed;
            receipt.committed_unix_ms = Some(now);
            atomic_write_json(&path, &receipt)?;
            continue;
        }
        if now.saturating_sub(receipt.authorized_unix_ms) > AUTHORIZED_RECEIPT_TTL_MS {
            fs::remove_file(&path)
                .map_err(|error| format!("Cannot prune stale marketplace authorization receipt: {error}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_receipt() -> MarketplaceProvenanceReceipt {
        MarketplaceProvenanceReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            origin: "marketplace".to_string(),
            state: ReceiptState::Authorized,
            mutation: MarketplaceMutationKind::Update,
            registry_source_id: "team".to_string(),
            registry_id: "team-registry".to_string(),
            registry_snapshot_sha256: "a".repeat(64),
            extension_id: "com.example.echo".to_string(),
            version: "1.2.3".to_string(),
            package_sha256: "b".repeat(64),
            manifest_sha256: "c".repeat(64),
            lease_created_unix_ms: 10,
            authorized_unix_ms: 20,
            committed_unix_ms: None,
        }
    }

    #[test]
    fn receipt_key_binds_all_immutable_distribution_identity() {
        let original = sample_receipt();
        let original_key = receipt_identity_key(&original);
        assert_eq!(original_key.len(), 64);

        let mut changed = original.clone();
        changed.registry_snapshot_sha256 = "d".repeat(64);
        assert_ne!(receipt_identity_key(&changed), original_key);

        changed = original.clone();
        changed.package_sha256 = "e".repeat(64);
        assert_ne!(receipt_identity_key(&changed), original_key);

        changed = original.clone();
        changed.manifest_sha256 = "f".repeat(64);
        assert_ne!(receipt_identity_key(&changed), original_key);
    }

    #[test]
    fn runtime_result_must_match_receipt_identity_exactly() {
        let receipt = sample_receipt();
        assert!(validate_result_identity(
            &receipt,
            "com.example.echo",
            "1.2.3",
            &"c".repeat(64),
            "team-registry",
        )
        .is_ok());
        assert!(validate_result_identity(
            &receipt,
            "com.example.other",
            "1.2.3",
            &"c".repeat(64),
            "team-registry",
        )
        .is_err());
        assert!(validate_result_identity(
            &receipt,
            "com.example.echo",
            "1.2.4",
            &"c".repeat(64),
            "team-registry",
        )
        .is_err());
        assert!(validate_result_identity(
            &receipt,
            "com.example.echo",
            "1.2.3",
            &"d".repeat(64),
            "team-registry",
        )
        .is_err());
        assert!(validate_result_identity(
            &receipt,
            "com.example.echo",
            "1.2.3",
            &"c".repeat(64),
            "other-registry",
        )
        .is_err());
    }
}
