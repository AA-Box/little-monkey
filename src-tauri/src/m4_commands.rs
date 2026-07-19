//! Thin Tauri command wrappers for `m4_services`. The root app must manage one
//! `M4CommandState` built with real crypto/keychain/network/approval/executor
//! adapters; this module intentionally provides no permissive default.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::m4_services::{
    plugin_workflow_id, plugin_workflow_marker, plugin_workflow_prefix,
    ActivePluginRuntimeSnapshot, ActiveSkillDescriptor, ApprovedInstallPreview, M4ServiceError,
    McpAppService, McpOAuthServerRegistration, OpenedMcpUiSession, PackageCatalogEntry,
    PackageInstallAuthorization, PackageRegistryService, PluginRuntimeDescriptor,
    UiActionApprovalChallenge, WorkflowHumanApprovalChallenge, WorkflowService,
};
use crate::mcp_app_core::{
    AuthorizedBridgeAction, McpUiManifest, OAuthAuthorizationPlan, OAuthCallback,
    OAuthTokenMetadata, SecretMaterial, UiBridgeRequest,
};
use crate::package_ecosystem::{
    AdditionalRegistryRecord, InstalledPackageState, PermissionApproval, PortablePackageExport,
    RegistrySnapshot, SemanticVersion,
};
use crate::workflow_core::{
    LegacyRecipeV1, NodeRunRecord, ReconciliationDecision, ReplayPlan, WorkflowDefinition,
    WorkflowIr, WorkflowRunHistory, WorkflowRunRequest,
};

pub struct M4CommandState {
    pub packages: Arc<PackageRegistryService>,
    pub mcp_apps: Arc<McpAppService>,
    pub workflows: Arc<WorkflowService>,
    workflow_browser: Option<Arc<crate::browser_worker::BrowserWorkflowAdapter>>,
    app_data_dir: Option<PathBuf>,
}

impl M4CommandState {
    pub fn new(
        packages: Arc<PackageRegistryService>,
        mcp_apps: Arc<McpAppService>,
        workflows: Arc<WorkflowService>,
    ) -> Self {
        Self {
            packages,
            mcp_apps,
            workflows,
            workflow_browser: None,
            app_data_dir: None,
        }
    }

    /// Builds the real filesystem/keychain/network/crypto/approval/daemon
    /// adapters under the supplied Tauri app-data directory.
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        let services = crate::m4_runtime::production_m4_services(app_data_dir)?;
        Ok(Self {
            packages: services.packages,
            mcp_apps: services.mcp_apps,
            workflows: services.workflows,
            workflow_browser: Some(services.workflow_browser),
            app_data_dir: Some(app_data_dir.to_path_buf()),
        })
    }

    /// Explicitly terminates workflow-owned browser processes before Tauri's
    /// hard exit path skips destructors. Idempotent and safe when constructed
    /// with test services that have no production browser adapter.
    pub fn shutdown_all_blocking(&self) -> Result<usize, String> {
        match &self.workflow_browser {
            Some(browser) => browser.shutdown_all(),
            None => Ok(0),
        }
    }

    /// Rebuilds the same read-only aggregate used by the desktop Plugin
    /// runtime panel. CLI callers use this method directly so MCP/OAuth and
    /// materialized-workflow health cannot drift into a second implementation.
    pub fn plugin_runtime(&self) -> Result<Vec<PluginRuntimeDescriptor>, String> {
        let configured_mcp_servers = match &self.app_data_dir {
            Some(app_data_dir) => {
                crate::mcp::load_config_impl(&app_data_dir.join("mcp_servers.json"))?
                    .servers
                    .into_iter()
                    .filter(|server| server.enabled)
                    .map(|server| {
                        (
                            server.id,
                            server
                                .tool_allowlist
                                .map(|tools| tools.into_iter().collect()),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            }
            None => BTreeMap::new(),
        };
        let mut oauth_server_ids = BTreeSet::new();
        let mut oauth_origins = BTreeSet::new();
        for registration in self.mcp_apps.oauth_servers().map_err(command_error)? {
            oauth_server_ids.insert(registration.client.server_id.clone());
            for endpoint in [
                registration.server.issuer,
                registration.server.authorization_endpoint,
                registration.server.token_endpoint,
            ] {
                if let Ok(url) = url::Url::parse(&endpoint) {
                    oauth_origins.insert(url.origin().ascii_serialization());
                }
            }
        }
        let activated_workflow_ids = self
            .workflows
            .list()
            .map_err(command_error)?
            .into_iter()
            .map(|workflow| workflow.workflow_id)
            .collect::<BTreeSet<_>>();
        self.packages
            .plugin_runtime(
                &configured_mcp_servers,
                &oauth_server_ids,
                &oauth_origins,
                &activated_workflow_ids,
            )
            .map_err(command_error)
    }
}

fn deactivate_package_workflows(state: &M4CommandState, package_id: &str) -> Result<usize, String> {
    let prefix = plugin_workflow_prefix(package_id);
    let marker = plugin_workflow_marker(package_id);
    let workflows = state.workflows.list().map_err(command_error)?;
    let mut removed = 0;
    for workflow in workflows
        .into_iter()
        .filter(|workflow| workflow.workflow_id.starts_with(&prefix))
    {
        if !workflow.name.ends_with(&marker) {
            return Err(format!(
                "Plugin workflow {} has an ownership marker mismatch; delete or restore it explicitly",
                workflow.workflow_id
            ));
        }
        state
            .workflows
            .unregister_persistent_triggers(&workflow.workflow_id)
            .map_err(command_error)?;
        state
            .workflows
            .delete(&workflow.workflow_id)
            .map_err(command_error)?;
        removed += 1;
    }
    Ok(removed)
}

fn command_error(error: M4ServiceError) -> String {
    error.to_string()
}

fn require_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Package and plugin mutations are allowed only from the main window".to_string())
    }
}

#[tauri::command]
pub fn m4_packages_seed_first_party(
    state: tauri::State<'_, M4CommandState>,
    now_unix_ms: u64,
) -> Result<Vec<PackageCatalogEntry>, String> {
    state
        .packages
        .seed_first_party(now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_import_portable(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    portable: PortablePackageExport,
    expected_bundle_sha256: Option<String>,
    now_unix_ms: u64,
) -> Result<PackageCatalogEntry, String> {
    require_main_window(&window)?;
    state
        .packages
        .import_portable(portable, expected_bundle_sha256.as_deref(), now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_catalog(
    state: tauri::State<'_, M4CommandState>,
    now_unix_ms: u64,
) -> Result<Vec<PackageCatalogEntry>, String> {
    state.packages.catalog(now_unix_ms).map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_installed(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<InstalledPackageState>, String> {
    state.packages.installed().map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_active_skills(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<ActiveSkillDescriptor>, String> {
    state.packages.active_skills().map_err(command_error)
}

#[tauri::command]
pub fn m4_plugins_active_snapshot(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<ActivePluginRuntimeSnapshot>, String> {
    state
        .packages
        .active_plugin_snapshots()
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_plugins_runtime(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<PluginRuntimeDescriptor>, String> {
    state.plugin_runtime()
}

#[tauri::command]
pub fn m4_plugins_activate_workflow(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    content_path: String,
) -> Result<WorkflowIr, String> {
    require_main_window(&window)?;
    let mut template = state
        .packages
        .plugin_workflow_template(&package_id, &content_path)
        .map_err(command_error)?;
    match state.workflows.load(&template.workflow_id) {
        Ok(existing) => {
            let marker = plugin_workflow_marker(&package_id);
            if !existing.name.ends_with(&marker) {
                return Err("Refusing to replace a workflow not owned by this plugin".to_string());
            }
            if existing == template {
                return state.workflows.validate(&template).map_err(command_error);
            }
            template.workflow_version = existing.workflow_version.saturating_add(1);
            state.workflows.update(template).map_err(command_error)
        }
        Err(M4ServiceError::NotFound(_)) => state.workflows.create(template).map_err(command_error),
        Err(error) => Err(command_error(error)),
    }
}

#[tauri::command]
pub fn m4_plugins_deactivate_workflow(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    content_path: String,
) -> Result<bool, String> {
    require_main_window(&window)?;
    let workflow_id = plugin_workflow_id(&package_id, &content_path);
    let marker = plugin_workflow_marker(&package_id);
    let existing = match state.workflows.load(&workflow_id) {
        Ok(existing) => existing,
        Err(M4ServiceError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(command_error(error)),
    };
    if !existing.name.ends_with(&marker) {
        return Err("Refusing to delete a workflow not owned by this plugin".to_string());
    }
    state
        .workflows
        .unregister_persistent_triggers(&workflow_id)
        .map_err(command_error)?;
    state
        .workflows
        .delete(&workflow_id)
        .map_err(command_error)?;
    Ok(true)
}

#[tauri::command]
pub fn m4_packages_preview(
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    version: SemanticVersion,
    now_unix_ms: u64,
) -> Result<ApprovedInstallPreview, String> {
    state
        .packages
        .preview(&package_id, version, now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_install(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    authorization: PackageInstallAuthorization,
    now_unix_ms: u64,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    state
        .packages
        .install(&authorization, now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_update(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    version: SemanticVersion,
    approval: Option<PermissionApproval>,
    now_unix_ms: u64,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    let updated = state
        .packages
        .update(&package_id, version, approval.as_ref(), now_unix_ms)
        .map_err(command_error)?;
    deactivate_package_workflows(state.inner(), &package_id)?;
    Ok(updated)
}

#[tauri::command]
pub fn m4_packages_set_enabled(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    enabled: bool,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    if !enabled {
        deactivate_package_workflows(state.inner(), &package_id)?;
    }
    let updated = state
        .packages
        .set_enabled(&package_id, enabled)
        .map_err(command_error)?;
    Ok(updated)
}

#[tauri::command]
pub fn m4_packages_pin(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    version: Option<SemanticVersion>,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    state
        .packages
        .pin(&package_id, version)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_rollback(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    let rolled_back = state
        .packages
        .rollback(&package_id)
        .map_err(command_error)?;
    deactivate_package_workflows(state.inner(), &package_id)?;
    Ok(rolled_back)
}

#[tauri::command]
pub fn m4_packages_uninstall(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    deactivate_package_workflows(state.inner(), &package_id)?;
    let uninstalled = state
        .packages
        .uninstall(&package_id)
        .map_err(command_error)?;
    Ok(uninstalled)
}

#[tauri::command]
pub fn m4_packages_export(
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
) -> Result<PortablePackageExport, String> {
    state.packages.export(&package_id).map_err(command_error)
}

#[tauri::command]
pub fn m4_packages_set_team_approved(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    package_id: String,
    team_approved: bool,
) -> Result<InstalledPackageState, String> {
    require_main_window(&window)?;
    state
        .packages
        .set_team_approved(&package_id, team_approved)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_registries_list(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<AdditionalRegistryRecord>, String> {
    state.packages.list_registry_sources().map_err(command_error)
}

#[tauri::command]
pub fn m4_registries_add(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    source_id: String,
    display_name: String,
    location: String,
    now_unix_ms: u64,
) -> Result<AdditionalRegistryRecord, String> {
    require_main_window(&window)?;
    state
        .packages
        .add_registry_source(source_id, display_name, location, now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_registries_remove(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    source_id: String,
) -> Result<bool, String> {
    require_main_window(&window)?;
    state
        .packages
        .remove_registry_source(&source_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_registries_verify(
    window: tauri::Window,
    state: tauri::State<'_, M4CommandState>,
    source_id: String,
    snapshot: RegistrySnapshot,
    now_unix_ms: u64,
) -> Result<AdditionalRegistryRecord, String> {
    require_main_window(&window)?;
    state
        .packages
        .verify_registry_source(&source_id, snapshot, now_unix_ms)
        .map_err(command_error)
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthCallbackInput {
    pub state: String,
    pub code: String,
    pub error: Option<String>,
}

#[tauri::command]
pub fn m4_mcp_oauth_register(
    state: tauri::State<'_, M4CommandState>,
    registration: McpOAuthServerRegistration,
) -> Result<(), String> {
    state
        .mcp_apps
        .register_oauth_server(registration)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_servers(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<McpOAuthServerRegistration>, String> {
    state.mcp_apps.oauth_servers().map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_begin(
    state: tauri::State<'_, M4CommandState>,
    server_id: String,
    now_unix_ms: u64,
    lifetime_ms: u64,
) -> Result<OAuthAuthorizationPlan, String> {
    state
        .mcp_apps
        .begin_oauth(&server_id, now_unix_ms, lifetime_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_complete(
    state: tauri::State<'_, M4CommandState>,
    server_id: String,
    callback: OAuthCallbackInput,
    now_unix_ms: u64,
) -> Result<OAuthTokenMetadata, String> {
    let code = SecretMaterial::new(callback.code).map_err(|error| error.to_string())?;
    state
        .mcp_apps
        .complete_oauth(
            &server_id,
            OAuthCallback {
                state: callback.state,
                code,
                error: callback.error,
            },
            now_unix_ms,
        )
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_refresh(
    state: tauri::State<'_, M4CommandState>,
    server_id: String,
    now_unix_ms: u64,
) -> Result<OAuthTokenMetadata, String> {
    state
        .mcp_apps
        .refresh_oauth(&server_id, now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_revoke(
    state: tauri::State<'_, M4CommandState>,
    server_id: String,
) -> Result<(), String> {
    state
        .mcp_apps
        .revoke_oauth(&server_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_oauth_metadata(
    state: tauri::State<'_, M4CommandState>,
    server_id: String,
) -> Result<Option<OAuthTokenMetadata>, String> {
    state
        .mcp_apps
        .token_metadata(&server_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_ui_open(
    state: tauri::State<'_, M4CommandState>,
    manifest: McpUiManifest,
    resource_bytes: Vec<u8>,
    granted_permissions: BTreeSet<String>,
) -> Result<OpenedMcpUiSession, String> {
    state
        .mcp_apps
        .open_ui_session(manifest, &resource_bytes, granted_permissions)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_ui_authorize_action(
    state: tauri::State<'_, M4CommandState>,
    session_id: String,
    presented_capability: String,
    request: UiBridgeRequest,
) -> Result<AuthorizedBridgeAction, String> {
    state
        .mcp_apps
        .authorize_ui_action(&session_id, presented_capability, request)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_ui_prepare_action(
    state: tauri::State<'_, M4CommandState>,
    session_id: String,
    presented_capability: String,
    request: UiBridgeRequest,
) -> Result<UiActionApprovalChallenge, String> {
    state
        .mcp_apps
        .prepare_ui_action(&session_id, presented_capability, request)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_ui_decide_action(
    state: tauri::State<'_, M4CommandState>,
    challenge_id: String,
    approved: bool,
) -> Result<(), String> {
    state
        .mcp_apps
        .decide_ui_action(&challenge_id, approved)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_mcp_ui_close(
    state: tauri::State<'_, M4CommandState>,
    session_id: String,
) -> Result<bool, String> {
    state
        .mcp_apps
        .close_ui_session(&session_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_list(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<WorkflowDefinition>, String> {
    state.workflows.list().map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_load(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
) -> Result<WorkflowDefinition, String> {
    state.workflows.load(&workflow_id).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_validate(
    state: tauri::State<'_, M4CommandState>,
    definition: WorkflowDefinition,
) -> Result<WorkflowIr, String> {
    state.workflows.validate(&definition).map_err(command_error)
}

/// Reloads dynamic MCP allowlists while retaining the fixed, effect-classed
/// production adapters. This keeps the visual editor live after MCP Settings
/// changes without weakening compile-time capability checks.
#[tauri::command]
pub fn m4_workflows_refresh_capabilities(
    state: tauri::State<'_, M4CommandState>,
) -> Result<(), String> {
    let app_data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the application data directory".to_string())?;
    crate::m4_runtime::refresh_production_workflow_capabilities(
        state.workflows.as_ref(),
        &app_data_dir,
    )
}

#[tauri::command]
pub fn m4_workflows_create(
    state: tauri::State<'_, M4CommandState>,
    definition: WorkflowDefinition,
) -> Result<WorkflowIr, String> {
    state.workflows.create(definition).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_update(
    state: tauri::State<'_, M4CommandState>,
    definition: WorkflowDefinition,
) -> Result<WorkflowIr, String> {
    state.workflows.update(definition).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_import_legacy(
    state: tauri::State<'_, M4CommandState>,
    recipe: LegacyRecipeV1,
) -> Result<WorkflowIr, String> {
    state.workflows.import_legacy(recipe).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_delete(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
) -> Result<(), String> {
    state.workflows.delete(&workflow_id).map_err(command_error)
}

#[tauri::command]
pub async fn m4_workflows_run(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
    request: WorkflowRunRequest,
) -> Result<WorkflowRunHistory, String> {
    let workflows = state.workflows.clone();
    tauri::async_runtime::spawn_blocking(move || workflows.run_workflow(&workflow_id, request))
        .await
        .map_err(|error| format!("workflow task join failed: {error}"))?
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_cancel(
    state: tauri::State<'_, M4CommandState>,
    run_id: String,
) -> Result<bool, String> {
    state.workflows.cancel(&run_id).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_prepare_approval(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
    run_id: String,
    node_id: String,
    summary: String,
) -> Result<WorkflowHumanApprovalChallenge, String> {
    state
        .workflows
        .prepare_human_approval(&workflow_id, &run_id, &node_id, &summary)
        .map_err(command_error)
}

#[tauri::command]
pub async fn m4_workflows_decide_approval(
    app: tauri::AppHandle,
    app_state: tauri::State<'_, crate::AppState>,
    state: tauri::State<'_, M4CommandState>,
    challenge_id: String,
    approved: bool,
) -> Result<bool, String> {
    if !approved {
        state
            .workflows
            .decide_human_approval(&challenge_id, false)
            .map_err(command_error)?;
        return Ok(false);
    }

    let challenge = state
        .workflows
        .human_approval_challenge(&challenge_id)
        .map_err(command_error)?;
    let template = crate::approval_chains::built_in_templates()
        .into_iter()
        .find(|candidate| candidate.id == "review_then_approve")
        .ok_or_else(|| "review_then_approve approval template is unavailable".to_string())?;
    let detail = format!(
        "Workflow: {}\nRun: {}\nNode: {}\nPolicy: {}\n\n{}",
        challenge.workflow_id,
        challenge.run_id,
        challenge.node_id,
        challenge.approval_policy_id,
        challenge.summary
    );
    let digest_payload = serde_json::to_vec(&(
        &challenge.workflow_id,
        &challenge.run_id,
        &challenge.node_id,
        &challenge.approval_policy_id,
        &challenge.summary_sha256,
    ))
    .map_err(|error| format!("encode workflow approval digest: {error}"))?;
    let operation_digest = format!("{:x}", Sha256::digest(digest_payload));
    let chain_approved = crate::approval_chains::run_approval_chain(
        &app,
        app_state.inner(),
        &template,
        operation_digest,
        detail,
    )
    .await?;
    state
        .workflows
        .decide_human_approval(&challenge_id, chain_approved)
        .map_err(command_error)?;
    Ok(chain_approved)
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowReplayResponse {
    pub plan: ReplayPlan,
    pub history: WorkflowRunHistory,
}

#[tauri::command]
pub async fn m4_workflows_replay(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
    source_run_id: String,
    boundary_node_id: String,
    replay_approval_granted: bool,
    request: WorkflowRunRequest,
) -> Result<WorkflowReplayResponse, String> {
    let workflows = state.workflows.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        workflows.replay(
            &workflow_id,
            &source_run_id,
            &boundary_node_id,
            replay_approval_granted,
            request,
        )
    })
    .await
    .map_err(|error| format!("workflow replay join failed: {error}"))?
    .map_err(command_error)?;
    Ok(WorkflowReplayResponse {
        plan: result.0,
        history: result.1,
    })
}

#[tauri::command]
pub fn m4_workflows_histories(
    state: tauri::State<'_, M4CommandState>,
) -> Result<Vec<WorkflowRunHistory>, String> {
    state.workflows.histories().map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_history(
    state: tauri::State<'_, M4CommandState>,
    run_id: String,
) -> Result<WorkflowRunHistory, String> {
    state.workflows.history(&run_id).map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_inspect_node(
    state: tauri::State<'_, M4CommandState>,
    run_id: String,
    node_id: String,
) -> Result<NodeRunRecord, String> {
    state
        .workflows
        .inspect_node(&run_id, &node_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_reconcile(
    state: tauri::State<'_, M4CommandState>,
    run_id: String,
    node_id: String,
    decision: ReconciliationDecision,
    now_unix_ms: u64,
) -> Result<WorkflowRunHistory, String> {
    state
        .workflows
        .reconcile(&run_id, &node_id, decision, now_unix_ms)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_register_triggers(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
) -> Result<Vec<String>, String> {
    state
        .workflows
        .register_persistent_triggers(&workflow_id)
        .map_err(command_error)
}

#[tauri::command]
pub fn m4_workflows_unregister_triggers(
    state: tauri::State<'_, M4CommandState>,
    workflow_id: String,
) -> Result<(), String> {
    state
        .workflows
        .unregister_persistent_triggers(&workflow_id)
        .map_err(command_error)
}
