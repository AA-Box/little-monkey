//! Fixed, typed desktop bridge for executable extensions.
//!
//! React cannot submit a CLI argv or generic command. Each lifecycle action is
//! named here and secrets are written directly to the shared OS keychain.

use crate::executable_extensions::{
    ActiveCapability, Approval, CapabilityKind, ExtensionDetail, ExtensionLogRow, ExtensionManager,
    ExtensionPreview, InvocationRequest, InvocationResult, PermissionKind,
};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKETPLACE_STAGE_PREFIX: &str = "little-monkey-marketplace-lmx-v1:";
const MARKETPLACE_CACHE_DIR: &str = "extension-marketplace-cache-v1";
const MAX_LMX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;
const MAX_LMX_FILES: usize = 128;
const MAX_LMX_PATH_CHARS: usize = 512;
const MAX_LMX_FILE_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_DECODED_BYTES: usize = 3 * 1024 * 1024;
const MAX_LMX_MANIFEST_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceStageRequest {
    raw_package: String,
    package_sha256: String,
    manifest_sha256: String,
    extension_id: String,
    version: String,
    registry_source_id: String,
    registry_snapshot_sha256: String,
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

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn require_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{label} is not a SHA-256 digest"));
    }
    Ok(())
}

fn canonical_json(value: &serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => serde_json::to_string(value)
            .map_err(|error| format!("Cannot encode canonical marketplace JSON: {error}")),
        serde_json::Value::Array(items) => {
            let encoded = items
                .iter()
                .map(canonical_json)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", encoded.join(",")))
        }
        serde_json::Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut fields = Vec::with_capacity(keys.len());
            for key in keys {
                let encoded_key = serde_json::to_string(key)
                    .map_err(|error| format!("Cannot encode marketplace JSON key: {error}"))?;
                fields.push(format!("{encoded_key}:{}", canonical_json(&object[key])?));
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

fn stage_marketplace_package(encoded: &str) -> Result<PathBuf, String> {
    let request: MarketplaceStageRequest = serde_json::from_str(encoded)
        .map_err(|error| format!("Invalid marketplace staging request: {error}"))?;
    require_sha256("Marketplace package digest", &request.package_sha256)?;
    require_sha256("Marketplace manifest digest", &request.manifest_sha256)?;
    require_sha256(
        "Marketplace registry snapshot digest",
        &request.registry_snapshot_sha256,
    )?;
    if request.registry_source_id.trim().is_empty() {
        return Err("Marketplace registry source id is empty".to_string());
    }
    let raw = request.raw_package.as_bytes();
    if raw.is_empty() || raw.len() > MAX_LMX_DOWNLOAD_BYTES {
        return Err("Marketplace .lmx exceeds its bounded encoded size".to_string());
    }
    if sha256_hex(raw) != request.package_sha256.to_ascii_lowercase() {
        return Err("Marketplace .lmx does not match the verified M4 package digest".to_string());
    }

    let envelope: MarketplaceEnvelope = serde_json::from_slice(raw)
        .map_err(|error| format!("Marketplace .lmx is not valid JSON: {error}"))?;
    if envelope.schema_version != 1 {
        return Err(format!(
            "Unsupported marketplace .lmx schema {}",
            envelope.schema_version
        ));
    }
    if envelope.files_base64.is_empty() || envelope.files_base64.len() > MAX_LMX_FILES {
        return Err("Marketplace .lmx has an invalid file count".to_string());
    }

    let manifest_canonical = canonical_json(&envelope.manifest)?;
    if manifest_canonical.len() > MAX_LMX_MANIFEST_BYTES {
        return Err("Marketplace extension manifest exceeds its metadata limit".to_string());
    }
    if sha256_hex(manifest_canonical.as_bytes()) != request.manifest_sha256.to_ascii_lowercase() {
        return Err("Marketplace manifest does not match the verified M4 manifest digest".to_string());
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
    if manifest_id != request.extension_id || manifest_version != request.version {
        return Err("Marketplace manifest identity/version disagrees with signed M4 metadata".to_string());
    }
    let component_path = envelope
        .manifest
        .get("component")
        .and_then(|value| value.get("path"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Marketplace manifest is missing component.path".to_string())?;
    let component = safe_marketplace_path(component_path)?;

    let mut decoded = Vec::with_capacity(envelope.files_base64.len());
    let mut total = 0usize;
    let mut collisions = BTreeSet::new();
    for (raw_path, encoded_bytes) in envelope.files_base64 {
        let relative = safe_marketplace_path(&raw_path)?;
        let key = relative.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        if !collisions.insert(key) {
            return Err(format!("Marketplace .lmx contains a colliding path: {raw_path}"));
        }
        if relative == Path::new("extension.json") {
            return Err("Marketplace .lmx must not contain a second extension.json".to_string());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded_bytes)
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

    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())?;
    let root = app_data.join(MARKETPLACE_CACHE_DIR);
    fs::create_dir_all(&root).map_err(|error| format!("Cannot create marketplace cache: {error}"))?;
    let directory = root.join(format!(
        "{}-{}",
        request.package_sha256.to_ascii_lowercase(),
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&directory)
        .map_err(|error| format!("Cannot create marketplace staging directory: {error}"))?;

    let write_result = (|| -> Result<(), String> {
        fs::write(directory.join("extension.json"), format!("{}\n", serde_json::to_string_pretty(&envelope.manifest).map_err(|error| format!("Cannot encode marketplace manifest: {error}"))?))
            .map_err(|error| format!("Cannot stage marketplace manifest: {error}"))?;
        for (relative, bytes) in decoded {
            let target = directory.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Cannot create marketplace package directory: {error}"))?;
            }
            fs::write(&target, bytes)
                .map_err(|error| format!("Cannot stage marketplace package file: {error}"))?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    Ok(directory)
}

fn resolve_discovery_source(source_path: String) -> Result<String, String> {
    let Some(encoded) = source_path.strip_prefix(MARKETPLACE_STAGE_PREFIX) else {
        return Ok(source_path);
    };
    Ok(stage_marketplace_package(encoded)?.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn extensions_discover(source_path: String) -> Result<ExtensionPreview, String> {
    manager()?.discover(resolve_discovery_source(source_path)?)
}

#[tauri::command]
pub async fn extensions_list() -> Result<Vec<ExtensionDetail>, String> {
    manager()?.list()
}

#[tauri::command]
pub async fn extensions_active_capabilities(
    kind: Option<CapabilityKind>,
) -> Result<Vec<ActiveCapability>, String> {
    manager()?.active_capabilities(kind)
}

#[tauri::command]
pub async fn extensions_inspect(extension_id: String) -> Result<ExtensionDetail, String> {
    manager()?.inspect(&extension_id)
}

#[tauri::command]
pub async fn extensions_install(
    source_path: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    manager()?.install(source_path, approval).await
}

#[tauri::command]
pub async fn extensions_validate(extension_id: String) -> Result<ExtensionDetail, String> {
    manager()?.validate_installed(&extension_id).await
}

#[tauri::command]
pub async fn extensions_set_enabled(
    extension_id: String,
    enabled: bool,
) -> Result<ExtensionDetail, String> {
    manager()?.set_enabled(&extension_id, enabled).await
}

#[tauri::command]
pub async fn extensions_set_running(
    extension_id: String,
    running: bool,
) -> Result<ExtensionDetail, String> {
    manager()?.set_running(&extension_id, running).await
}

#[tauri::command]
pub async fn extensions_preview_update(source_path: String) -> Result<ExtensionPreview, String> {
    manager()?.preview_update(source_path)
}

#[tauri::command]
pub async fn extensions_update(
    source_path: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    manager()?.update(source_path, approval).await
}

#[tauri::command]
pub async fn extensions_rollback(extension_id: String) -> Result<ExtensionDetail, String> {
    manager()?.rollback(&extension_id).await
}

#[tauri::command]
pub async fn extensions_uninstall(extension_id: String) -> Result<(), String> {
    manager()?.uninstall(&extension_id)
}

#[tauri::command]
pub async fn extensions_status(extension_id: String) -> Result<ExtensionDetail, String> {
    manager()?.inspect(&extension_id)
}

#[tauri::command]
pub async fn extensions_logs(
    extension_id: String,
    limit: u32,
) -> Result<Vec<ExtensionLogRow>, String> {
    manager()?.logs(&extension_id, limit)
}

#[tauri::command]
pub async fn extensions_set_config(
    extension_id: String,
    values: BTreeMap<String, serde_json::Value>,
) -> Result<ExtensionDetail, String> {
    manager()?.set_config(&extension_id, values).await
}

/// The only desktop boundary where an extension secret exists as plaintext.
/// It is never put in a sidecar argument, returned, logged, or persisted in the
/// registry; the UI receives only the slot's configured boolean on refresh.
#[tauri::command]
pub async fn extensions_set_secret(
    extension_id: String,
    slot_id: String,
    secret: String,
) -> Result<(), String> {
    manager()?.set_secret(&extension_id, &slot_id, &secret)
}

#[tauri::command]
pub async fn extensions_remove_secret(extension_id: String, slot_id: String) -> Result<(), String> {
    manager()?.remove_secret(&extension_id, &slot_id)
}

#[tauri::command]
pub async fn extensions_invoke(
    state: tauri::State<'_, crate::m3_commands::M3CommandState>,
    request: InvocationRequest,
) -> Result<InvocationResult, String> {
    manager()?
        .with_model_hub(state.hub.clone())
        .invoke(request)
        .await
}

#[tauri::command]
pub async fn extensions_cancel(invocation_id: String) -> Result<bool, String> {
    manager()?.cancel_invocation(&invocation_id)
}

#[tauri::command]
pub async fn extensions_webhooks(
    extension_id: String,
) -> Result<Vec<crate::daemon_commands::ExtensionWebhookStatus>, String> {
    crate::daemon_commands::extension_webhooks(&extension_id).await
}

#[tauri::command]
pub async fn extensions_register_webhook(
    trigger_id: String,
    extension_id: String,
    handler_id: String,
    secret: String,
    max_skew_ms: u64,
) -> Result<Vec<crate::daemon_commands::ExtensionWebhookStatus>, String> {
    let detail = manager()?.inspect(&extension_id)?;
    if !detail.manifest.capabilities.iter().any(|capability| {
        capability.capability_id == handler_id && capability.kind == CapabilityKind::Channel
    }) {
        return Err("Webhook handler is not a declared channel capability".to_string());
    }
    if !detail.permissions.iter().any(|permission| {
        permission.granted
            && permission.kind == PermissionKind::WebhookReceive
            && permission.scope == handler_id
    }) {
        return Err("Webhook handler lacks its exact ingress grant".to_string());
    }
    crate::daemon_commands::extension_webhook_register(
        &trigger_id,
        &extension_id,
        &handler_id,
        &detail.active_version,
        &detail.trust.manifest_sha256,
        secret,
        max_skew_ms,
    )
    .await?;
    crate::daemon_commands::extension_webhooks(&extension_id).await
}

#[tauri::command]
pub async fn extensions_remove_webhook(
    trigger_id: String,
    extension_id: String,
) -> Result<Vec<crate::daemon_commands::ExtensionWebhookStatus>, String> {
    crate::daemon_commands::extension_webhook_remove(&trigger_id, &extension_id).await?;
    crate::daemon_commands::extension_webhooks(&extension_id).await
}
