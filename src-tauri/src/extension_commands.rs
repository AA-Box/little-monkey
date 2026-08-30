//! Fixed, typed desktop bridge for executable extensions.
//!
//! React cannot submit a CLI argv or generic command. Each lifecycle action is
//! named here and secrets are written directly to the shared OS keychain.

use crate::executable_extensions::{
    ActiveCapability, Approval, CapabilityKind, ExtensionDetail, ExtensionLogRow, ExtensionManager,
    ExtensionPreview, InvocationRequest, InvocationResult, PermissionKind,
};
use std::collections::BTreeMap;

#[path = "marketplace_commands.rs"]
pub(crate) mod marketplace_commands;
#[path = "marketplace_provenance.rs"]
pub(crate) mod marketplace_provenance;
#[path = "marketplace_staging_integrity.rs"]
pub(crate) mod marketplace_staging_integrity;

const MARKETPLACE_PREPARE_PREFIX: &str = "little-monkey-marketplace-prepare:v2:";
const MARKETPLACE_HANDLE_PREFIX: &str = "little-monkey-marketplace:v2:";

fn manager() -> Result<ExtensionManager, String> {
    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())?;
    ExtensionManager::new(app_data)
}

#[tauri::command]
pub async fn extensions_discover(
    window: tauri::Window,
    state: tauri::State<'_, crate::m4_commands::M4CommandState>,
    source_path: String,
) -> Result<ExtensionPreview, String> {
    if let Some(encoded) = source_path.strip_prefix(MARKETPLACE_PREPARE_PREFIX) {
        let request = serde_json::from_str(encoded)
            .map_err(|error| format!("Invalid marketplace prepare request: {error}"))?;
        let preview = marketplace_commands::marketplace_prepare_extension(window, state, request).await?;
        marketplace_staging_integrity::validate_handle(&preview.source_path)?;
        return Ok(preview);
    }
    if source_path.starts_with(MARKETPLACE_HANDLE_PREFIX) {
        marketplace_staging_integrity::validate_handle(&source_path)?;
        return marketplace_commands::marketplace_preview_install(state, source_path).await;
    }
    manager()?.discover(source_path)
}

#[tauri::command]
pub async fn extensions_list(
    window: tauri::Window,
    state: tauri::State<'_, crate::m4_commands::M4CommandState>,
    refresh_marketplace: Option<bool>,
    cleanup_marketplace_handle: Option<String>,
) -> Result<Vec<ExtensionDetail>, String> {
    if let Some(handle) = cleanup_marketplace_handle {
        if window.label() != "main" {
            return Err("Marketplace staging cleanup is allowed only from the main window".to_string());
        }
        if !handle.starts_with(MARKETPLACE_HANDLE_PREFIX) {
            return Err("Invalid marketplace staging handle".to_string());
        }
        marketplace_commands::marketplace_cleanup_extension(handle)?;
    }
    if refresh_marketplace.unwrap_or(false) {
        marketplace_commands::marketplace_refresh_registries(window, state).await?;
    }
    let installed = manager()?.list()?;
    marketplace_provenance::reconcile(&installed)?;
    Ok(installed)
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
    window: tauri::Window,
    state: tauri::State<'_, crate::m4_commands::M4CommandState>,
    source_path: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    if source_path.starts_with(MARKETPLACE_HANDLE_PREFIX) {
        marketplace_staging_integrity::validate_handle(&source_path)?;
        // Persist the native lease identity before mutation.  On a crash after
        // runtime success, `extensions_list` reconciles this authorization into
        // a committed receipt by matching the installed manifest/trust identity.
        let receipt = marketplace_provenance::authorize_from_handle(
            &source_path,
            marketplace_provenance::MarketplaceMutationKind::Install,
        )?;
        let detail = marketplace_commands::marketplace_install_extension(
            window,
            state,
            source_path,
            approval,
        )
        .await?;
        marketplace_provenance::commit(&receipt, &detail)?;
        return Ok(detail);
    }
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
pub async fn extensions_preview_update(
    state: tauri::State<'_, crate::m4_commands::M4CommandState>,
    source_path: String,
) -> Result<ExtensionPreview, String> {
    if source_path.starts_with(MARKETPLACE_HANDLE_PREFIX) {
        marketplace_staging_integrity::validate_handle(&source_path)?;
        return marketplace_commands::marketplace_preview_update(state, source_path).await;
    }
    manager()?.preview_update(source_path)
}

#[tauri::command]
pub async fn extensions_update(
    window: tauri::Window,
    state: tauri::State<'_, crate::m4_commands::M4CommandState>,
    source_path: String,
    approval: Approval,
) -> Result<ExtensionDetail, String> {
    if source_path.starts_with(MARKETPLACE_HANDLE_PREFIX) {
        marketplace_staging_integrity::validate_handle(&source_path)?;
        let receipt = marketplace_provenance::authorize_from_handle(
            &source_path,
            marketplace_provenance::MarketplaceMutationKind::Update,
        )?;
        let detail = marketplace_commands::marketplace_update_extension(
            window,
            state,
            source_path,
            approval,
        )
        .await?;
        marketplace_provenance::commit(&receipt, &detail)?;
        return Ok(detail);
    }
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
    manager()?.set_config(&extension_id, values)
}

#[tauri::command]
pub async fn extensions_set_secret(
    extension_id: String,
    slot_id: String,
    secret: String,
) -> Result<(), String> {
    manager()?.set_secret(&extension_id, &slot_id, &secret)
}

#[tauri::command]
pub async fn extensions_remove_secret(
    extension_id: String,
    slot_id: String,
) -> Result<(), String> {
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
