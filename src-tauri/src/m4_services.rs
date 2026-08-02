//! Tauri-free M4 application services. These APIs are shared by desktop
//! commands and CLI callers; every network, crypto, keychain, approval,
//! workflow-node, clock, and persistent-trigger effect is injected.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::mcp_app_core::{
    authorize_ui_bridge_action, begin_oauth, build_ui_host_plan, complete_oauth,
    prepare_ui_bridge_action, refresh_oauth, revoke_oauth, route_tools, verify_ui_resource_bytes,
    AuthorizedBridgeAction, BridgeCapability, McpCoreError,
    McpToolDescriptor, McpUiHostPlan, McpUiManifest, OAuthAuthorizationPlan, OAuthCallback,
    OAuthClientConfig, OAuthFlowStore, OAuthSecretVault, OAuthSecurityProvider,
    OAuthServerMetadata, OAuthTokenMetadata, OAuthTransport, PreparedBridgeAction, RoutedTool,
    ToolRouterModel, ToolRoutingPolicy, UiActionApprovalGate, UiBridgeRequest,
};
use crate::package_ecosystem::{
    install_preview, signed_first_party_catalog, verify_package, verify_registry_snapshot,
    AdditionalRegistryRecord, AdditionalRegistrySource, ConnectorAuthKind, ContentKind,
    InstallEnvironment, InstallPreview, InstallTrustPolicy, InstalledPackageState,
    McpRequirementKind, PackageBundle, PackageError, PackageKind, PackageLimits, PackageManifest,
    PackagePermission, PackageStore, PermissionApproval, PortablePackageExport, RegistrySnapshot,
    SemanticVersion, SignatureVerifier, TrustEvidence, TrustStore, VerifiedPackage,
    VerifiedRegistryState,
};
use crate::process_table::{
    ExitStatus, ProcessExit, ProcessKind, ProcessProjection, ProcessProjector, ProcessState,
};
use crate::workflow_core::{
    adapt_legacy_recipe, compile_workflow, plan_replay, reconcile_node, DaemonCapability,
    HeadlessWorkflowExecutor, LegacyRecipeV1, NodeRunRecord, NodeRunStatus, ReconciliationDecision,
    ReplayPlan, WorkflowCapabilityCatalog, WorkflowClock, WorkflowDefinition, WorkflowError,
    WorkflowIr, WorkflowNodeExecutor, WorkflowRunHistory, WorkflowRunRequest, WorkflowRunStatus,
    WorkflowTrigger,
};

pub const M4_SERVICE_CONTRACT_VERSION: u32 = 1;
pub const M4_TRIGGER_ADAPTER_CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M4ServiceError {
    Package(String),
    Mcp(String),
    Workflow(String),
    Io(String),
    Conflict(String),
    NotFound(String),
    Dependency(String),
}

impl fmt::Display for M4ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package(message) => write!(formatter, "package service: {message}"),
            Self::Mcp(message) => write!(formatter, "MCP app service: {message}"),
            Self::Workflow(message) => write!(formatter, "workflow service: {message}"),
            Self::Io(message) => write!(formatter, "M4 storage: {message}"),
            Self::Conflict(message) => write!(formatter, "M4 conflict: {message}"),
            Self::NotFound(message) => write!(formatter, "M4 not found: {message}"),
            Self::Dependency(message) => write!(formatter, "M4 dependency: {message}"),
        }
    }
}

impl std::error::Error for M4ServiceError {}

impl From<PackageError> for M4ServiceError {
    fn from(error: PackageError) -> Self {
        Self::Package(error.to_string())
    }
}

impl From<McpCoreError> for M4ServiceError {
    fn from(error: McpCoreError) -> Self {
        Self::Mcp(error.to_string())
    }
}

impl From<WorkflowError> for M4ServiceError {
    fn from(error: WorkflowError) -> Self {
        Self::Workflow(error.to_string())
    }
}

impl From<std::io::Error> for M4ServiceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for M4ServiceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Io(error.to_string())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, M4ServiceError> {
    mutex
        .lock()
        .map_err(|_| M4ServiceError::Io(format!("{label} lock poisoned")))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageCatalogEntry {
    pub manifest: PackageManifest,
    pub bundle_sha256: String,
    pub trust: Option<TrustEvidence>,
    pub available: bool,
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovedInstallPreview {
    pub preview: InstallPreview,
    pub approval_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageInstallAuthorization {
    pub package_id: String,
    pub version: SemanticVersion,
    pub approval_digest: String,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryRefreshResult {
    pub registry_sequence: u64,
    pub revoked_installed_package_ids: Vec<String>,
}

/// Runtime-safe view of one enabled, verified skill package. The command is
/// derived from the package id rather than trusting instruction content, and
/// the instruction bytes are loaded from the verified active bundle on every
/// discovery so disable/rollback/revocation takes effect before the next turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSkillDescriptor {
    pub package_id: String,
    pub version: SemanticVersion,
    pub name: String,
    pub command: String,
    pub description: String,
    pub instructions: String,
    pub content_sha256: String,
    pub permissions: BTreeSet<PackagePermission>,
}

/// Aggregate health for a declarative plugin. "Needs setup" is deliberately
/// distinct from "blocked": a missing separately-approved MCP/OAuth binding
/// never causes the package manager to install or authorize it implicitly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntimeHealth {
    Healthy,
    NeedsSetup,
    Disabled,
    Blocked,
    Corrupt,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginComponentKind {
    Skill,
    Assistant,
    Connector,
    Instructions,
    Prompt,
    Persona,
    Rule,
    Workflow,
    KnowledgeTemplate,
    UiResource,
    McpRequirement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginComponentState {
    Active,
    Available,
    NeedsSetup,
    Disabled,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginComponentDescriptor {
    pub component_id: String,
    pub kind: PluginComponentKind,
    pub label: String,
    pub source_path: Option<String>,
    pub content_sha256: Option<String>,
    pub activation_id: Option<String>,
    pub state: PluginComponentState,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRuntimeDescriptor {
    pub package_id: String,
    pub version: Option<SemanticVersion>,
    pub name: String,
    pub description: String,
    pub kind: PackageKind,
    pub health: PluginRuntimeHealth,
    pub enabled: bool,
    pub signed: bool,
    pub bundle_sha256: Option<String>,
    pub pinned_version: Option<SemanticVersion>,
    pub rollback_target: Option<SemanticVersion>,
    pub rollback_healthy: bool,
    pub permissions: BTreeSet<PackagePermission>,
    pub components: Vec<PluginComponentDescriptor>,
    pub issues: Vec<String>,
}

/// Verified, enabled package snapshot for runtime consumers. It contains the
/// declarative manifest plus bounded UTF-8 content only; UI HTML/SVG is never
/// returned here and must still go through the opaque-origin MCP App host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivePluginRuntimeSnapshot {
    pub package_id: String,
    pub version: SemanticVersion,
    pub bundle_sha256: String,
    pub manifest: PackageManifest,
    pub text_content: BTreeMap<String, String>,
}

const MAX_ACTIVE_PLUGIN_TEXT_BYTES: usize = 2 * 1024 * 1024;

fn plugin_component_state(
    enabled: bool,
    blocked: bool,
    ready: PluginComponentState,
) -> PluginComponentState {
    if blocked {
        PluginComponentState::Blocked
    } else if !enabled {
        PluginComponentState::Disabled
    } else {
        ready
    }
}

fn slug_component(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_dash = false;
        } else if !previous_dash && !output.is_empty() {
            output.push('-');
            previous_dash = true;
        }
        if output.len() >= maximum {
            break;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "plugin".to_string()
    } else {
        output
    }
}

/// Stable namespace used only for workflows materialized from verified
/// package templates. Including both package and path digests prevents a
/// package from colliding with ordinary user workflow identifiers.
pub fn plugin_workflow_prefix(package_id: &str) -> String {
    let tail = package_id.rsplit('.').next().unwrap_or(package_id);
    format!(
        "plugin:{}:{}:",
        slug_component(tail, 24),
        &sha256(package_id.as_bytes())[..12]
    )
}

pub fn plugin_workflow_id(package_id: &str, content_path: &str) -> String {
    format!(
        "{}{}",
        plugin_workflow_prefix(package_id),
        &sha256(content_path.as_bytes())[..16]
    )
}

pub fn plugin_workflow_marker(package_id: &str) -> String {
    format!(" [plugin:{package_id}]")
}

pub struct PackageRegistryService {
    store: PackageStore,
    catalog: Mutex<BTreeMap<(String, SemanticVersion), PackageBundle>>,
    registry: Mutex<Option<VerifiedRegistryState>>,
    trust_store: TrustStore,
    environment: InstallEnvironment,
    policy: InstallTrustPolicy,
    limits: PackageLimits,
    verifier: Arc<dyn SignatureVerifier>,
}

impl PackageRegistryService {
    pub fn new(
        root: impl AsRef<Path>,
        trust_store: TrustStore,
        environment: InstallEnvironment,
        policy: InstallTrustPolicy,
        limits: PackageLimits,
        verifier: Arc<dyn SignatureVerifier>,
    ) -> Result<Self, M4ServiceError> {
        trust_store.validate()?;
        limits.validate()?;
        Ok(Self {
            store: PackageStore::new(root)?,
            catalog: Mutex::new(BTreeMap::new()),
            registry: Mutex::new(None),
            trust_store,
            environment,
            policy,
            limits,
            verifier,
        })
    }

    pub fn refresh_registry(
        &self,
        snapshot: RegistrySnapshot,
        now_unix_ms: u64,
    ) -> Result<RegistryRefreshResult, M4ServiceError> {
        let mut registry = lock(&self.registry, "package registry")?;
        let verified = verify_registry_snapshot(
            &snapshot,
            &self.trust_store,
            registry.as_ref(),
            self.verifier.as_ref(),
            now_unix_ms,
        )?;
        *registry = Some(verified.clone());
        drop(registry);

        let bundles = lock(&self.catalog, "package catalog")?.clone();
        let mut revoked = Vec::new();
        for state in self.store.list_installed()? {
            let Some(active) = state.active_version else {
                continue;
            };
            let Some(bundle) = bundles.get(&(state.package_id.clone(), active)) else {
                continue;
            };
            if matches!(
                verify_package(
                    bundle,
                    &self.trust_store,
                    Some(&verified),
                    &self.environment,
                    &self.policy,
                    &self.limits,
                    self.verifier.as_ref(),
                    now_unix_ms,
                ),
                Err(PackageError::Revoked(_))
            ) {
                self.store.mark_revoked(&state.package_id)?;
                revoked.push(state.package_id);
            }
        }
        revoked.sort();
        Ok(RegistryRefreshResult {
            registry_sequence: snapshot.sequence,
            revoked_installed_package_ids: revoked,
        })
    }

    pub fn import_bundle(
        &self,
        bundle: PackageBundle,
        now_unix_ms: u64,
    ) -> Result<PackageCatalogEntry, M4ServiceError> {
        let verified = self.verify_bundle(&bundle, now_unix_ms)?;
        let key = (
            verified.manifest().package_id.clone(),
            verified.manifest().version,
        );
        lock(&self.catalog, "package catalog")?.insert(key, bundle);
        Ok(Self::entry_from_verified(&verified))
    }

    /// Imports the portable, checksum-bound representation used by the UI.
    /// The optional out-of-band digest lets a registry page or release note
    /// pin the exact bytes independently from the digest embedded in the
    /// downloaded file.
    pub fn import_portable(
        &self,
        portable: PortablePackageExport,
        expected_bundle_sha256: Option<&str>,
        now_unix_ms: u64,
    ) -> Result<PackageCatalogEntry, M4ServiceError> {
        if let Some(expected) = expected_bundle_sha256 {
            if expected != portable.bundle_sha256 {
                return Err(M4ServiceError::Conflict(
                    "portable package does not match the expected bundle digest".to_string(),
                ));
            }
        }
        let bundle = portable.into_bundle(&self.limits)?;
        self.import_bundle(bundle, now_unix_ms)
    }

    pub fn seed_first_party(
        &self,
        now_unix_ms: u64,
    ) -> Result<Vec<PackageCatalogEntry>, M4ServiceError> {
        let (_, snapshot, bundles) = signed_first_party_catalog()?;
        let already_verified = lock(&self.registry, "package registry")?
            .as_ref()
            .is_some_and(|state| state.snapshot() == &snapshot);
        if !already_verified {
            let refresh = self.refresh_registry(snapshot, now_unix_ms)?;
            debug_assert_eq!(refresh.registry_sequence, 1);
        }
        bundles
            .into_iter()
            .map(|bundle| self.import_bundle(bundle, now_unix_ms))
            .collect()
    }

    pub fn catalog(&self, now_unix_ms: u64) -> Result<Vec<PackageCatalogEntry>, M4ServiceError> {
        let bundles = lock(&self.catalog, "package catalog")?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut entries = bundles
            .into_iter()
            .map(|bundle| match self.verify_bundle(&bundle, now_unix_ms) {
                Ok(verified) => Self::entry_from_verified(&verified),
                Err(error) => {
                    let bundle_sha256 = bundle
                        .validate(&self.limits)
                        .unwrap_or_else(|_| "invalid".to_string());
                    PackageCatalogEntry {
                        manifest: bundle.manifest,
                        bundle_sha256,
                        trust: None,
                        available: false,
                        validation_error: Some(error.to_string()),
                    }
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.manifest
                .package_id
                .cmp(&right.manifest.package_id)
                .then_with(|| left.manifest.version.cmp(&right.manifest.version))
        });
        Ok(entries)
    }

    pub fn preview(
        &self,
        package_id: &str,
        version: SemanticVersion,
        now_unix_ms: u64,
    ) -> Result<ApprovedInstallPreview, M4ServiceError> {
        let verified = self.catalog_package(package_id, version, now_unix_ms)?;
        let installed = self.store.installed(package_id)?;
        let preview = install_preview(&verified, installed.as_ref())?;
        let approval_digest = sha256(&serde_json::to_vec(&preview)?);
        Ok(ApprovedInstallPreview {
            preview,
            approval_digest,
        })
    }

    pub fn install(
        &self,
        authorization: &PackageInstallAuthorization,
        now_unix_ms: u64,
    ) -> Result<InstalledPackageState, M4ServiceError> {
        let approved = self.preview(
            &authorization.package_id,
            authorization.version,
            now_unix_ms,
        )?;
        if !authorization.approved || authorization.approval_digest != approved.approval_digest {
            return Err(M4ServiceError::Conflict(
                "install authorization is denied or does not match the current preview".to_string(),
            ));
        }
        let verified = self.catalog_package(
            &authorization.package_id,
            authorization.version,
            now_unix_ms,
        )?;
        Ok(self.store.install(&verified)?)
    }

    pub fn update(
        &self,
        package_id: &str,
        version: SemanticVersion,
        approval: Option<&PermissionApproval>,
        now_unix_ms: u64,
    ) -> Result<InstalledPackageState, M4ServiceError> {
        let verified = self.catalog_package(package_id, version, now_unix_ms)?;
        Ok(self.store.update(&verified, approval)?)
    }

    pub fn set_enabled(
        &self,
        package_id: &str,
        enabled: bool,
    ) -> Result<InstalledPackageState, M4ServiceError> {
        Ok(self.store.set_enabled(package_id, enabled)?)
    }

    /// Sets the local "team approved" flag on an installed package. This is
    /// deliberately not gated behind any role/permission check here — it is
    /// a plain, locally-observed toggle intended for `PackageKind::Collection`
    /// packages so a separate Team Mode feature, present or not, never has a
    /// hard dependency on this field.
    pub fn set_team_approved(
        &self,
        package_id: &str,
        team_approved: bool,
    ) -> Result<InstalledPackageState, M4ServiceError> {
        Ok(self.store.set_team_approved(package_id, team_approved)?)
    }

    pub fn pin(
        &self,
        package_id: &str,
        version: Option<SemanticVersion>,
    ) -> Result<InstalledPackageState, M4ServiceError> {
        Ok(self.store.pin(package_id, version)?)
    }

    pub fn rollback(&self, package_id: &str) -> Result<InstalledPackageState, M4ServiceError> {
        Ok(self.store.rollback(package_id)?)
    }

    pub fn uninstall(&self, package_id: &str) -> Result<InstalledPackageState, M4ServiceError> {
        Ok(self.store.uninstall(package_id)?)
    }

    pub fn export(&self, package_id: &str) -> Result<PortablePackageExport, M4ServiceError> {
        Ok(self.store.export_active(package_id)?)
    }

    /// Lists every user-added registry source (the roadmap's "private/team
    /// catalog"), including ones that have never successfully verified.
    pub fn list_registry_sources(&self) -> Result<Vec<AdditionalRegistryRecord>, M4ServiceError> {
        Ok(self.store.list_registry_sources()?)
    }

    pub fn add_registry_source(
        &self,
        source_id: String,
        display_name: String,
        location: String,
        now_unix_ms: u64,
    ) -> Result<AdditionalRegistryRecord, M4ServiceError> {
        Ok(self.store.add_registry_source(AdditionalRegistrySource {
            source_id,
            display_name,
            location,
            added_unix_ms: now_unix_ms,
        })?)
    }

    pub fn remove_registry_source(&self, source_id: &str) -> Result<bool, M4ServiceError> {
        Ok(self.store.remove_registry_source(source_id)?)
    }

    /// Verifies a caller-supplied registry snapshot for one added source
    /// through the exact same Ed25519 trust chain
    /// ([`verify_registry_snapshot`]) as the built-in first-party registry —
    /// never a bypass. Packages from this source only become visible once
    /// this call succeeds; a failed verification is persisted as an error
    /// (returned in `last_verification_error` on the record below, not as a
    /// service error) and never marks the source trusted, but a previously
    /// verified snapshot is retained rather than discarded.
    pub fn verify_registry_source(
        &self,
        source_id: &str,
        snapshot: RegistrySnapshot,
        now_unix_ms: u64,
    ) -> Result<AdditionalRegistryRecord, M4ServiceError> {
        let previous = self
            .store
            .list_registry_sources()?
            .into_iter()
            .find(|record| record.source.source_id == source_id)
            .ok_or_else(|| {
                M4ServiceError::NotFound(format!("registry source {source_id} is not registered"))
            })?
            .verified;
        match verify_registry_snapshot(
            &snapshot,
            &self.trust_store,
            previous.as_ref(),
            self.verifier.as_ref(),
            now_unix_ms,
        ) {
            Ok(verified) => Ok(self
                .store
                .record_registry_verification(source_id, Some(verified), None)?),
            Err(error) => Ok(self.store.record_registry_verification(
                source_id,
                None,
                Some(error.to_string()),
            )?),
        }
    }

    /// Lists installed package states, excluding tombstoned (uninstalled) entries.
    pub fn installed(&self) -> Result<Vec<InstalledPackageState>, M4ServiceError> {
        Ok(self
            .store
            .list_installed()?
            .into_iter()
            .filter(|state| !state.tombstoned)
            .collect())
    }

    /// Produces one consolidated runtime/health view across all installed
    /// declarative packages. Consumers pass their current MCP/OAuth/workflow
    /// bindings; the package layer only reports missing bindings and never
    /// creates them as a side effect.
    pub fn plugin_runtime(
        &self,
        configured_mcp_servers: &BTreeMap<String, Option<BTreeSet<String>>>,
        oauth_server_ids: &BTreeSet<String>,
        oauth_origins: &BTreeSet<String>,
        activated_workflow_ids: &BTreeSet<String>,
    ) -> Result<Vec<PluginRuntimeDescriptor>, M4ServiceError> {
        // Tombstones are durable reinstall/audit metadata, not installed
        // plugins. Exposing them here created a ghost blocked runtime card:
        // export_active correctly returned NotInstalled, but the UI still
        // rendered the historical package as corrupt. Building on installed()
        // keeps this view aligned with the Settings installed list, so a
        // non-tombstoned state without an active version still surfaces as a
        // blocked runtime instead of silently disappearing.
        let states = self.installed()?;
        let enabled_packages = states
            .iter()
            .filter(|state| state.enabled && !state.revoked)
            .map(|state| state.package_id.clone())
            .collect::<BTreeSet<_>>();
        let mut plugins = Vec::with_capacity(states.len());

        for state in states {
            let active = state.active_version;
            let blocked = state.revoked || active.is_none();
            let rollback_target = state
                .activation_history
                .iter()
                .rev()
                .copied()
                .find(|version| Some(*version) != active);
            let rollback_healthy = rollback_target.is_some_and(|version| {
                self.store
                    .export_version(&state.package_id, version)
                    .is_ok()
            });
            let mut issues = Vec::new();
            if rollback_target.is_some() && !rollback_healthy {
                issues.push("The previous activation exists in history but its immutable cache failed validation.".to_string());
            }

            let portable = match self.store.export_active(&state.package_id) {
                Ok(portable) => portable,
                Err(error) => {
                    issues.push(format!("Active package cache failed validation: {error}"));
                    plugins.push(PluginRuntimeDescriptor {
                        package_id: state.package_id.clone(),
                        version: active,
                        name: state.package_id,
                        description: "Installed package metadata is unavailable.".to_string(),
                        kind: PackageKind::Collection,
                        health: if blocked {
                            PluginRuntimeHealth::Blocked
                        } else {
                            PluginRuntimeHealth::Corrupt
                        },
                        enabled: state.enabled,
                        signed: false,
                        bundle_sha256: None,
                        pinned_version: state.pinned_version,
                        rollback_target,
                        rollback_healthy,
                        permissions: state.approved_permissions,
                        components: Vec::new(),
                        issues,
                    });
                    continue;
                }
            };
            let bundle_sha256 = portable.bundle_sha256.clone();
            let bundle = match portable.into_bundle(&self.limits) {
                Ok(bundle) => bundle,
                Err(error) => {
                    issues.push(format!("Active package bundle failed validation: {error}"));
                    plugins.push(PluginRuntimeDescriptor {
                        package_id: state.package_id.clone(),
                        version: active,
                        name: state.package_id,
                        description: "Installed package content is unavailable.".to_string(),
                        kind: PackageKind::Collection,
                        health: PluginRuntimeHealth::Corrupt,
                        enabled: state.enabled,
                        signed: false,
                        bundle_sha256: Some(bundle_sha256),
                        pinned_version: state.pinned_version,
                        rollback_target,
                        rollback_healthy,
                        permissions: state.approved_permissions,
                        components: Vec::new(),
                        issues,
                    });
                    continue;
                }
            };
            let manifest = &bundle.manifest;
            let signed = active
                .and_then(|version| state.versions.get(&version))
                .is_some_and(|cached| cached.trust.signed);
            let mut components = Vec::new();
            let mut needs_setup = false;

            let base_state =
                plugin_component_state(state.enabled, blocked, PluginComponentState::Active);
            match manifest.kind {
                PackageKind::Skill => components.push(PluginComponentDescriptor {
                    component_id: format!("{}:skill", manifest.package_id),
                    kind: PluginComponentKind::Skill,
                    label: manifest.display_name.clone(),
                    source_path: None,
                    content_sha256: Some(bundle_sha256.clone()),
                    activation_id: skill_command(&manifest.package_id).ok(),
                    state: base_state,
                    detail: "Available as a deterministic slash command; turn permissions remain enforced.".to_string(),
                }),
                PackageKind::Assistant => {
                    let missing_skills = manifest
                        .assistant
                        .as_ref()
                        .map(|assistant| {
                            assistant
                                .skill_package_ids
                                .difference(&enabled_packages)
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let inactive_starters = manifest
                        .assistant
                        .as_ref()
                        .map(|assistant| {
                            assistant
                                .starter_workflow_paths
                                .iter()
                                .filter(|path| {
                                    !activated_workflow_ids
                                        .contains(&plugin_workflow_id(&manifest.package_id, path))
                                })
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if !missing_skills.is_empty() {
                        needs_setup = true;
                        issues.push(format!(
                            "Assistant dependencies are disabled or missing: {}",
                            missing_skills.join(", ")
                        ));
                    }
                    if !inactive_starters.is_empty() {
                        needs_setup = true;
                        issues.push(format!(
                            "Assistant starter workflows still need activation: {}",
                            inactive_starters.join(", ")
                        ));
                    }
                    let assistant_ready = missing_skills.is_empty() && inactive_starters.is_empty();
                    components.push(PluginComponentDescriptor {
                        component_id: format!("{}:assistant", manifest.package_id),
                        kind: PluginComponentKind::Assistant,
                        label: manifest.display_name.clone(),
                        source_path: manifest
                            .assistant
                            .as_ref()
                            .map(|assistant| assistant.persona_content_path.clone()),
                        content_sha256: Some(bundle_sha256.clone()),
                        activation_id: Some(manifest.package_id.clone()),
                        state: plugin_component_state(
                            state.enabled,
                            blocked,
                            if assistant_ready {
                                PluginComponentState::Active
                            } else {
                                PluginComponentState::NeedsSetup
                            },
                        ),
                        detail: if assistant_ready {
                            "Assistant composition and its declared skill dependencies are ready.".to_string()
                        } else {
                            "Enable declared skills and activate starter workflows before using this assistant."
                                .to_string()
                        },
                    });
                }
                PackageKind::Connector => {
                    let connector_slug = manifest
                        .package_id
                        .rsplit('.')
                        .next()
                        .unwrap_or(&manifest.package_id);
                    let oauth_missing = manifest.connector.as_ref().is_some_and(|connector| {
                        connector.auth == ConnectorAuthKind::OAuth
                            && !oauth_server_ids.contains(&manifest.package_id)
                            && !oauth_server_ids.contains(connector_slug)
                            && connector
                                .allowed_origins
                                .iter()
                                .all(|origin| !oauth_origins.contains(origin))
                    });
                    if oauth_missing {
                        needs_setup = true;
                        issues.push("Connector requires a separately approved OAuth connection.".to_string());
                    }
                    components.push(PluginComponentDescriptor {
                        component_id: format!("{}:connector", manifest.package_id),
                        kind: PluginComponentKind::Connector,
                        label: manifest.display_name.clone(),
                        source_path: None,
                        content_sha256: Some(bundle_sha256.clone()),
                        activation_id: Some(manifest.package_id.clone()),
                        state: plugin_component_state(
                            state.enabled,
                            blocked,
                            if oauth_missing {
                                PluginComponentState::NeedsSetup
                            } else {
                                PluginComponentState::Active
                            },
                        ),
                        detail: if oauth_missing {
                            "Register and authorize a matching OAuth origin before using this connector.".to_string()
                        } else {
                            "Connector contract, origin allowlist and operation permissions are active.".to_string()
                        },
                    });
                }
                PackageKind::Collection => {}
            }

            for reference in &manifest.content {
                let (kind, ready, activation_id, detail) = match reference.kind {
                    ContentKind::Instructions => (
                        PluginComponentKind::Instructions,
                        PluginComponentState::Active,
                        None,
                        "Verified instruction content is available to this plugin.".to_string(),
                    ),
                    ContentKind::Prompt => (
                        PluginComponentKind::Prompt,
                        PluginComponentState::Active,
                        None,
                        "Verified prompt content is available to this plugin.".to_string(),
                    ),
                    ContentKind::Persona => (
                        PluginComponentKind::Persona,
                        PluginComponentState::Active,
                        None,
                        "Verified persona content is available to assistant composition.".to_string(),
                    ),
                    ContentKind::Rule => (
                        PluginComponentKind::Rule,
                        PluginComponentState::Active,
                        None,
                        "Rule is active in the package runtime snapshot and cannot bypass host permissions.".to_string(),
                    ),
                    ContentKind::WorkflowTemplate => {
                        let workflow_id = plugin_workflow_id(&manifest.package_id, &reference.path);
                        let active = activated_workflow_ids.contains(&workflow_id);
                        (
                            PluginComponentKind::Workflow,
                            if active {
                                PluginComponentState::Active
                            } else {
                                PluginComponentState::Available
                            },
                            Some(workflow_id),
                            if active {
                                "Workflow template is materialized in the durable workflow store.".to_string()
                            } else {
                                "Template is verified and ready for explicit activation.".to_string()
                            },
                        )
                    }
                    ContentKind::KnowledgeTemplate => (
                        PluginComponentKind::KnowledgeTemplate,
                        PluginComponentState::Available,
                        None,
                        "Knowledge template is verified and available for explicit import.".to_string(),
                    ),
                    ContentKind::UiResource => (
                        PluginComponentKind::UiResource,
                        PluginComponentState::Available,
                        None,
                        "UI resource remains inert until opened in an opaque-origin MCP App sandbox.".to_string(),
                    ),
                };
                components.push(PluginComponentDescriptor {
                    component_id: format!("{}:{}", manifest.package_id, reference.path),
                    kind,
                    label: reference.path.clone(),
                    source_path: Some(reference.path.clone()),
                    content_sha256: Some(reference.sha256.clone()),
                    activation_id,
                    state: plugin_component_state(state.enabled, blocked, ready),
                    detail,
                });
            }

            for requirement in &manifest.mcp_requirements {
                let satisfied = match requirement.kind {
                    McpRequirementKind::ExistingServer => requirement
                        .server_id
                        .as_ref()
                        .and_then(|server_id| {
                            configured_mcp_servers.get(server_id).map(|allowlist| {
                                allowlist
                                    .as_ref()
                                    .is_none_or(|tools| requirement.required_tools.is_subset(tools))
                            })
                        })
                        .unwrap_or(false),
                    McpRequirementKind::RemoteHttp => requirement
                        .remote_origin
                        .as_ref()
                        .is_some_and(|origin| oauth_origins.contains(origin)),
                };
                if !satisfied {
                    needs_setup = true;
                    issues.push(format!(
                        "MCP requirement {} still needs separate configuration/approval.",
                        requirement.requirement_id
                    ));
                }
                components.push(PluginComponentDescriptor {
                    component_id: format!(
                        "{}:mcp:{}",
                        manifest.package_id, requirement.requirement_id
                    ),
                    kind: PluginComponentKind::McpRequirement,
                    label: requirement.requirement_id.clone(),
                    source_path: None,
                    content_sha256: None,
                    activation_id: requirement.server_id.clone(),
                    state: plugin_component_state(
                        state.enabled,
                        blocked,
                        if satisfied {
                            PluginComponentState::Active
                        } else {
                            PluginComponentState::NeedsSetup
                        },
                    ),
                    detail: if satisfied {
                        "Required MCP binding is configured; ordinary tool allowlists and approvals still apply.".to_string()
                    } else {
                        "Configure this MCP server/OAuth binding separately; package activation cannot approve it.".to_string()
                    },
                });
            }

            let health = if blocked {
                PluginRuntimeHealth::Blocked
            } else if !state.enabled {
                PluginRuntimeHealth::Disabled
            } else if needs_setup {
                PluginRuntimeHealth::NeedsSetup
            } else {
                PluginRuntimeHealth::Healthy
            };
            plugins.push(PluginRuntimeDescriptor {
                package_id: manifest.package_id.clone(),
                version: active,
                name: manifest.display_name.clone(),
                description: manifest.description.clone(),
                kind: manifest.kind,
                health,
                enabled: state.enabled,
                signed,
                bundle_sha256: Some(bundle_sha256),
                pinned_version: state.pinned_version,
                rollback_target,
                rollback_healthy,
                permissions: state.approved_permissions,
                components,
                issues,
            });
        }
        plugins.sort_by(|left, right| left.package_id.cmp(&right.package_id));
        Ok(plugins)
    }

    /// Loads one active workflow template only after revalidating the
    /// immutable bundle and checking that its owning package is enabled.
    pub fn plugin_workflow_template(
        &self,
        package_id: &str,
        content_path: &str,
    ) -> Result<WorkflowDefinition, M4ServiceError> {
        let state = self
            .store
            .installed(package_id)?
            .filter(|state| !state.tombstoned)
            .ok_or_else(|| M4ServiceError::NotFound(format!("package {package_id}")))?;
        if !state.enabled || state.revoked {
            return Err(M4ServiceError::Conflict(format!(
                "package {package_id} must be enabled and non-revoked"
            )));
        }
        let bundle = self
            .store
            .export_active(package_id)?
            .into_bundle(&self.limits)?;
        let reference = bundle
            .manifest
            .content
            .iter()
            .find(|reference| {
                reference.path == content_path && reference.kind == ContentKind::WorkflowTemplate
            })
            .ok_or_else(|| {
                M4ServiceError::NotFound(format!(
                    "workflow template {content_path} in package {package_id}"
                ))
            })?;
        if reference.media_type != "application/json" {
            return Err(M4ServiceError::Conflict(
                "workflow templates must use application/json".to_string(),
            ));
        }
        let bytes = bundle.files.get(content_path).ok_or_else(|| {
            M4ServiceError::Conflict("workflow template content is missing".to_string())
        })?;
        let mut definition: WorkflowDefinition = serde_json::from_slice(bytes)?;
        definition.workflow_id = plugin_workflow_id(package_id, content_path);
        let marker = plugin_workflow_marker(package_id);
        let available = 256_usize.saturating_sub(marker.len());
        definition.name = format!(
            "{}{}",
            definition.name.chars().take(available).collect::<String>(),
            marker
        );
        Ok(definition)
    }

    pub fn active_skills(&self) -> Result<Vec<ActiveSkillDescriptor>, M4ServiceError> {
        let mut skills = Vec::new();
        let mut commands = BTreeMap::<String, String>::new();
        for state in self.store.list_installed()? {
            if !state.enabled || state.revoked || state.tombstoned {
                continue;
            }
            let Some(version) = state.active_version else {
                continue;
            };
            let export = self.store.export_active(&state.package_id)?;
            let content_sha256 = export.bundle_sha256.clone();
            let bundle = export.into_bundle(&self.limits)?;
            if bundle.manifest.kind != PackageKind::Skill {
                continue;
            }
            let mut instruction_parts = Vec::new();
            for reference in &bundle.manifest.content {
                if !matches!(
                    reference.kind,
                    ContentKind::Instructions | ContentKind::Prompt
                ) {
                    continue;
                }
                let bytes = bundle.files.get(&reference.path).ok_or_else(|| {
                    M4ServiceError::Conflict(format!(
                        "active skill {} is missing {}",
                        state.package_id, reference.path
                    ))
                })?;
                let text = std::str::from_utf8(bytes).map_err(|_| {
                    M4ServiceError::Conflict(format!(
                        "active skill {} instruction file is not UTF-8",
                        state.package_id
                    ))
                })?;
                instruction_parts.push(text.trim().to_string());
            }
            if instruction_parts.is_empty() {
                return Err(M4ServiceError::Conflict(format!(
                    "active skill {} has no instruction content",
                    state.package_id
                )));
            }
            let command = skill_command(&state.package_id)?;
            if let Some(existing) = commands.insert(command.clone(), state.package_id.clone()) {
                return Err(M4ServiceError::Conflict(format!(
                    "skill command /{command} is ambiguous between {existing} and {}",
                    state.package_id
                )));
            }
            skills.push(ActiveSkillDescriptor {
                package_id: state.package_id,
                version,
                name: bundle.manifest.display_name,
                command,
                description: bundle.manifest.description,
                instructions: instruction_parts.join("\n\n"),
                content_sha256,
                permissions: bundle.manifest.permissions,
            });
        }
        skills.sort_by(|left, right| left.command.cmp(&right.command));
        Ok(skills)
    }

    /// Collision-free aggregate registry used by assistant, connector, rule,
    /// workflow, and knowledge-template consumers. Package enable/disable,
    /// rollback, revocation, and uninstall all take effect on the next call
    /// because each snapshot is rebuilt from the verified active cache.
    pub fn active_plugin_snapshots(
        &self,
    ) -> Result<Vec<ActivePluginRuntimeSnapshot>, M4ServiceError> {
        let mut snapshots = Vec::new();
        for state in self.store.list_installed()? {
            if !state.enabled || state.revoked || state.tombstoned {
                continue;
            }
            let Some(version) = state.active_version else {
                continue;
            };
            let portable = self.store.export_active(&state.package_id)?;
            let bundle_sha256 = portable.bundle_sha256.clone();
            let bundle = portable.into_bundle(&self.limits)?;
            let mut total = 0_usize;
            let mut text_content = BTreeMap::new();
            for reference in &bundle.manifest.content {
                if reference.kind == ContentKind::UiResource {
                    continue;
                }
                let bytes = bundle.files.get(&reference.path).ok_or_else(|| {
                    M4ServiceError::Conflict(format!(
                        "active plugin {} is missing {}",
                        state.package_id, reference.path
                    ))
                })?;
                let Ok(text) = std::str::from_utf8(bytes) else {
                    continue;
                };
                total = total.checked_add(bytes.len()).ok_or_else(|| {
                    M4ServiceError::Conflict("active plugin text size overflow".to_string())
                })?;
                if total > MAX_ACTIVE_PLUGIN_TEXT_BYTES {
                    return Err(M4ServiceError::Conflict(format!(
                        "active plugin {} exceeds the runtime text snapshot limit",
                        state.package_id
                    )));
                }
                text_content.insert(reference.path.clone(), text.to_string());
            }
            snapshots.push(ActivePluginRuntimeSnapshot {
                package_id: state.package_id,
                version,
                bundle_sha256,
                manifest: bundle.manifest,
                text_content,
            });
        }
        snapshots.sort_by(|left, right| left.package_id.cmp(&right.package_id));
        Ok(snapshots)
    }

    fn catalog_package(
        &self,
        package_id: &str,
        version: SemanticVersion,
        now_unix_ms: u64,
    ) -> Result<VerifiedPackage, M4ServiceError> {
        let bundle = lock(&self.catalog, "package catalog")?
            .get(&(package_id.to_string(), version))
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("package {package_id} {version}")))?;
        self.verify_bundle(&bundle, now_unix_ms)
    }

    fn verify_bundle(
        &self,
        bundle: &PackageBundle,
        now_unix_ms: u64,
    ) -> Result<VerifiedPackage, M4ServiceError> {
        let registry = lock(&self.registry, "package registry")?.clone();
        Ok(verify_package(
            bundle,
            &self.trust_store,
            registry.as_ref(),
            &self.environment,
            &self.policy,
            &self.limits,
            self.verifier.as_ref(),
            now_unix_ms,
        )?)
    }

    fn entry_from_verified(package: &VerifiedPackage) -> PackageCatalogEntry {
        PackageCatalogEntry {
            manifest: package.manifest().clone(),
            bundle_sha256: package.bundle_sha256().to_string(),
            trust: Some(package.trust().clone()),
            available: true,
            validation_error: None,
        }
    }
}

fn skill_command(package_id: &str) -> Result<String, M4ServiceError> {
    let tail = package_id.rsplit('.').next().unwrap_or(package_id);
    let mut command = String::new();
    let mut previous_dash = false;
    for character in tail.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            command.push(character);
            previous_dash = false;
        } else if !previous_dash && !command.is_empty() {
            command.push('-');
            previous_dash = true;
        }
        if command.len() >= 32 {
            break;
        }
    }
    while command.ends_with('-') {
        command.pop();
    }
    if command.is_empty() {
        Err(M4ServiceError::Conflict(format!(
            "skill package id {package_id} cannot form a slash command"
        )))
    } else {
        Ok(command)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpOAuthServerRegistration {
    pub server: OAuthServerMetadata,
    pub client: OAuthClientConfig,
}

pub trait McpUiSessionIssuer: Send + Sync {
    /// Must return a unique session id and a high-entropy capability.
    fn issue(&self) -> Result<(String, BridgeCapability), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiActionApprovalChallenge {
    pub challenge_id: String,
    pub session_id: String,
    pub action_id: String,
    pub action_target: String,
    pub required_permission: String,
    pub payload_summary_sha256: String,
}

pub trait UiActionApprovalBroker: UiActionApprovalGate {
    fn prepare(&self, action: &PreparedBridgeAction) -> Result<UiActionApprovalChallenge, String>;
    fn decide(&self, challenge_id: &str, approved: bool) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenedMcpUiSession {
    pub session_id: String,
    pub bridge_capability: String,
    pub host_plan: McpUiHostPlan,
}

#[derive(Debug, Clone)]
struct McpUiSessionRecord {
    manifest: McpUiManifest,
    expected_capability_sha256: String,
    granted_permissions: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct McpOAuthStoreRecord {
    contract_version: u32,
    sequence: u64,
    oauth_servers: BTreeMap<String, McpOAuthServerRegistration>,
    token_metadata: BTreeMap<String, OAuthTokenMetadata>,
    payload_sha256: String,
}

pub struct McpAppService {
    oauth_servers: Mutex<BTreeMap<String, McpOAuthServerRegistration>>,
    token_metadata: Mutex<BTreeMap<String, OAuthTokenMetadata>>,
    ui_sessions: Mutex<BTreeMap<String, McpUiSessionRecord>>,
    security: Arc<dyn OAuthSecurityProvider>,
    vault: Arc<dyn OAuthSecretVault>,
    flows: Arc<dyn OAuthFlowStore>,
    transport: Arc<dyn OAuthTransport>,
    ui_issuer: Arc<dyn McpUiSessionIssuer>,
    approval_gate: Arc<dyn UiActionApprovalBroker>,
    persistence_root: Option<PathBuf>,
    persistence_gate: Mutex<()>,
}

impl McpAppService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        security: Arc<dyn OAuthSecurityProvider>,
        vault: Arc<dyn OAuthSecretVault>,
        flows: Arc<dyn OAuthFlowStore>,
        transport: Arc<dyn OAuthTransport>,
        ui_issuer: Arc<dyn McpUiSessionIssuer>,
        approval_gate: Arc<dyn UiActionApprovalBroker>,
    ) -> Self {
        Self {
            oauth_servers: Mutex::new(BTreeMap::new()),
            token_metadata: Mutex::new(BTreeMap::new()),
            ui_sessions: Mutex::new(BTreeMap::new()),
            security,
            vault,
            flows,
            transport,
            ui_issuer,
            approval_gate,
            persistence_root: None,
            persistence_gate: Mutex::new(()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_persistent(
        root: impl AsRef<Path>,
        security: Arc<dyn OAuthSecurityProvider>,
        vault: Arc<dyn OAuthSecretVault>,
        flows: Arc<dyn OAuthFlowStore>,
        transport: Arc<dyn OAuthTransport>,
        ui_issuer: Arc<dyn McpUiSessionIssuer>,
        approval_gate: Arc<dyn UiActionApprovalBroker>,
    ) -> Result<Self, M4ServiceError> {
        let root = root.as_ref();
        if root.exists() && fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(M4ServiceError::Io(
                "MCP OAuth state directory cannot be a symlink".to_string(),
            ));
        }
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let root = fs::canonicalize(root)?;
        let restored = Self::load_latest_oauth_record(&root)?;
        let mut service = Self::new(security, vault, flows, transport, ui_issuer, approval_gate);
        if let Some(record) = restored {
            if !Self::validate_oauth_record(&record) {
                return Err(M4ServiceError::Io(
                    "newest MCP OAuth state record failed integrity validation".to_string(),
                ));
            }
            service.oauth_servers = Mutex::new(record.oauth_servers);
            service.token_metadata = Mutex::new(record.token_metadata);
        }
        service.persistence_root = Some(root);
        Ok(service)
    }

    pub fn register_oauth_server(
        &self,
        registration: McpOAuthServerRegistration,
    ) -> Result<(), M4ServiceError> {
        registration.server.validate()?;
        registration.client.validate(&registration.server)?;
        let server_id = registration.client.server_id.clone();
        let mut servers = lock(&self.oauth_servers, "OAuth server registry")?;
        if servers.contains_key(&server_id) {
            return Err(M4ServiceError::Conflict(format!(
                "OAuth server {server_id} is already registered"
            )));
        }
        servers.insert(server_id.clone(), registration);
        drop(servers);
        if let Err(error) = self.persist_oauth_state() {
            lock(&self.oauth_servers, "OAuth server registry")?.remove(&server_id);
            return Err(error);
        }
        Ok(())
    }

    /// Returns the restart-safe, non-secret OAuth registrations available to
    /// the desktop UI. Token material never leaves the configured vault.
    pub fn oauth_servers(&self) -> Result<Vec<McpOAuthServerRegistration>, M4ServiceError> {
        Ok(lock(&self.oauth_servers, "OAuth server registry")?
            .values()
            .cloned()
            .collect())
    }

    pub fn begin_oauth(
        &self,
        server_id: &str,
        now_unix_ms: u64,
        lifetime_ms: u64,
    ) -> Result<OAuthAuthorizationPlan, M4ServiceError> {
        let registration = self.oauth_registration(server_id)?;
        Ok(begin_oauth(
            &registration.server,
            &registration.client,
            self.security.as_ref(),
            self.vault.as_ref(),
            self.flows.as_ref(),
            now_unix_ms,
            lifetime_ms,
        )?)
    }

    pub fn complete_oauth(
        &self,
        server_id: &str,
        callback: OAuthCallback,
        now_unix_ms: u64,
    ) -> Result<OAuthTokenMetadata, M4ServiceError> {
        let registration = self.oauth_registration(server_id)?;
        let metadata = complete_oauth(
            &registration.server,
            &registration.client,
            callback,
            self.vault.as_ref(),
            self.flows.as_ref(),
            self.transport.as_ref(),
            now_unix_ms,
        )?;
        lock(&self.token_metadata, "OAuth token metadata")?
            .insert(server_id.to_string(), metadata.clone());
        if let Err(error) = self.persist_oauth_state() {
            lock(&self.token_metadata, "OAuth token metadata")?.remove(server_id);
            let _ = self.vault.delete_tokens(&metadata.token_reference);
            return Err(error);
        }
        Ok(metadata)
    }

    pub fn refresh_oauth(
        &self,
        server_id: &str,
        now_unix_ms: u64,
    ) -> Result<OAuthTokenMetadata, M4ServiceError> {
        let registration = self.oauth_registration(server_id)?;
        let current = lock(&self.token_metadata, "OAuth token metadata")?
            .get(server_id)
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("OAuth tokens for {server_id}")))?;
        let updated = refresh_oauth(
            &registration.server,
            &registration.client,
            &current,
            self.vault.as_ref(),
            self.transport.as_ref(),
            now_unix_ms,
        )?;
        lock(&self.token_metadata, "OAuth token metadata")?
            .insert(server_id.to_string(), updated.clone());
        self.persist_oauth_state()?;
        Ok(updated)
    }

    pub fn revoke_oauth(&self, server_id: &str) -> Result<(), M4ServiceError> {
        let registration = self.oauth_registration(server_id)?;
        let current = lock(&self.token_metadata, "OAuth token metadata")?
            .get(server_id)
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("OAuth tokens for {server_id}")))?;
        revoke_oauth(
            &registration.server,
            &registration.client,
            &current,
            self.vault.as_ref(),
            self.transport.as_ref(),
        )?;
        lock(&self.token_metadata, "OAuth token metadata")?.remove(server_id);
        self.persist_oauth_state()
    }

    pub fn token_metadata(
        &self,
        server_id: &str,
    ) -> Result<Option<OAuthTokenMetadata>, M4ServiceError> {
        Ok(lock(&self.token_metadata, "OAuth token metadata")?
            .get(server_id)
            .cloned())
    }

    pub fn open_ui_session(
        &self,
        manifest: McpUiManifest,
        resource_bytes: &[u8],
        granted_permissions: BTreeSet<String>,
    ) -> Result<OpenedMcpUiSession, M4ServiceError> {
        verify_ui_resource_bytes(&manifest, resource_bytes)?;
        let host_plan = build_ui_host_plan(&manifest)?;
        let (session_id, capability) =
            self.ui_issuer.issue().map_err(M4ServiceError::Dependency)?;
        if session_id.is_empty() || session_id.len() > 160 {
            return Err(M4ServiceError::Dependency(
                "UI session issuer returned an invalid session id".to_string(),
            ));
        }
        let record = McpUiSessionRecord {
            manifest,
            expected_capability_sha256: capability.hash(),
            granted_permissions,
        };
        let mut sessions = lock(&self.ui_sessions, "MCP UI sessions")?;
        if sessions.contains_key(&session_id) {
            return Err(M4ServiceError::Conflict(
                "UI session issuer reused an active session id".to_string(),
            ));
        }
        sessions.insert(session_id.clone(), record);
        drop(sessions);
        Ok(OpenedMcpUiSession {
            session_id,
            bridge_capability: capability.expose().to_string(),
            host_plan,
        })
    }

    pub fn authorize_ui_action(
        &self,
        session_id: &str,
        presented_capability: String,
        request: UiBridgeRequest,
    ) -> Result<AuthorizedBridgeAction, M4ServiceError> {
        let record = lock(&self.ui_sessions, "MCP UI sessions")?
            .get(session_id)
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("MCP UI session {session_id}")))?;
        let capability = BridgeCapability::new(presented_capability)?;
        Ok(authorize_ui_bridge_action(
            &record.manifest,
            session_id,
            &record.expected_capability_sha256,
            &capability,
            &record.granted_permissions,
            request,
            self.approval_gate.as_ref(),
        )?)
    }

    pub fn prepare_ui_action(
        &self,
        session_id: &str,
        presented_capability: String,
        request: UiBridgeRequest,
    ) -> Result<UiActionApprovalChallenge, M4ServiceError> {
        let record = lock(&self.ui_sessions, "MCP UI sessions")?
            .get(session_id)
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("MCP UI session {session_id}")))?;
        let capability = BridgeCapability::new(presented_capability)?;
        let prepared = prepare_ui_bridge_action(
            &record.manifest,
            session_id,
            &record.expected_capability_sha256,
            &capability,
            &record.granted_permissions,
            request,
        )?;
        self.approval_gate
            .prepare(&prepared)
            .map_err(M4ServiceError::Dependency)
    }

    pub fn decide_ui_action(
        &self,
        challenge_id: &str,
        approved: bool,
    ) -> Result<(), M4ServiceError> {
        self.approval_gate
            .decide(challenge_id, approved)
            .map_err(M4ServiceError::Dependency)
    }

    pub fn close_ui_session(&self, session_id: &str) -> Result<bool, M4ServiceError> {
        Ok(lock(&self.ui_sessions, "MCP UI sessions")?
            .remove(session_id)
            .is_some())
    }

    pub fn route_tools(
        &self,
        query: &str,
        catalog: &[McpToolDescriptor],
        policy: &ToolRoutingPolicy,
        router: Option<&dyn ToolRouterModel>,
    ) -> Result<Vec<RoutedTool>, M4ServiceError> {
        Ok(route_tools(query, catalog, policy, router)?)
    }

    fn oauth_registration(
        &self,
        server_id: &str,
    ) -> Result<McpOAuthServerRegistration, M4ServiceError> {
        lock(&self.oauth_servers, "OAuth server registry")?
            .get(server_id)
            .cloned()
            .ok_or_else(|| M4ServiceError::NotFound(format!("OAuth server {server_id}")))
    }

    fn persist_oauth_state(&self) -> Result<(), M4ServiceError> {
        let Some(root) = &self.persistence_root else {
            return Ok(());
        };
        let _guard = lock(&self.persistence_gate, "MCP OAuth persistence")?;
        let oauth_servers = lock(&self.oauth_servers, "OAuth server registry")?.clone();
        let token_metadata = lock(&self.token_metadata, "OAuth token metadata")?.clone();
        let sequence = Self::load_latest_oauth_record(root)?
            .map_or(1, |record| record.sequence.saturating_add(1));
        let payload_sha256 = sha256(&serde_json::to_vec(&(&oauth_servers, &token_metadata))?);
        let record = McpOAuthStoreRecord {
            contract_version: M4_SERVICE_CONTRACT_VERSION,
            sequence,
            oauth_servers,
            token_metadata,
            payload_sha256,
        };
        let path = root.join(format!(
            "record-{sequence:020}-{}.json",
            Uuid::new_v4().simple()
        ));
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(M4ServiceError::Io(
                "MCP OAuth metadata exceeds 4 MiB".to_string(),
            ));
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(root)
    }

    fn load_latest_oauth_record(
        root: &Path,
    ) -> Result<Option<McpOAuthStoreRecord>, M4ServiceError> {
        let mut entries = fs::read_dir(root)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_type()
                    .is_ok_and(|kind| kind.is_file() && !kind.is_symlink())
                    && entry.file_name().to_string_lossy().starts_with("record-")
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        let Some(entry) = entries.pop() else {
            return Ok(None);
        };
        if entry.metadata()?.len() > 4 * 1024 * 1024 {
            return Err(M4ServiceError::Io(
                "MCP OAuth metadata record exceeds 4 MiB".to_string(),
            ));
        }
        let record = serde_json::from_slice(&fs::read(entry.path())?)?;
        Ok(Some(record))
    }

    fn validate_oauth_record(record: &McpOAuthStoreRecord) -> bool {
        record.contract_version == M4_SERVICE_CONTRACT_VERSION
            && record.sequence > 0
            && record.oauth_servers.iter().all(|(id, registration)| {
                id == &registration.client.server_id
                    && registration.server.validate().is_ok()
                    && registration.client.validate(&registration.server).is_ok()
            })
            && record
                .token_metadata
                .keys()
                .all(|id| record.oauth_servers.contains_key(id))
            && serde_json::to_vec(&(&record.oauth_servers, &record.token_metadata))
                .map(|bytes| sha256(&bytes) == record.payload_sha256)
                .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowTriggerBatch {
    pub contract_version: u32,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub definition_sha256: String,
    pub triggers: Vec<WorkflowTrigger>,
}

/// Adapter boundary implemented by the M6 daemon/remote-runner layer. A batch
/// replacement must be atomic: either every persistent trigger is registered
/// for the definition digest or the previous batch remains active.
pub trait PersistentWorkflowTriggerRegistrar: Send + Sync {
    fn replace_batch(&self, batch: &WorkflowTriggerBatch) -> Result<Vec<String>, String>;
    fn remove_workflow(&self, workflow_id: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowHumanApprovalChallenge {
    pub challenge_id: String,
    pub workflow_id: String,
    pub run_id: String,
    pub node_id: String,
    pub approval_policy_id: String,
    /// Bounded evidence shown to the person deciding. The digest remains the
    /// authoritative executor binding.
    pub summary: String,
    pub summary_sha256: String,
}

/// Shared by the trusted UI service and the production node executor. An
/// approval is bound to the exact run/workflow/node/policy/summary tuple and
/// is consumed once; the model cannot create or expand approvals.
pub trait WorkflowHumanApprovalBroker: Send + Sync {
    fn prepare(
        &self,
        workflow_id: &str,
        run_id: &str,
        node_id: &str,
        approval_policy_id: &str,
        summary: &str,
        summary_sha256: &str,
    ) -> Result<WorkflowHumanApprovalChallenge, String>;
    fn get(&self, challenge_id: &str) -> Result<Option<WorkflowHumanApprovalChallenge>, String>;
    fn decide(&self, challenge_id: &str, approved: bool) -> Result<(), String>;
    fn consume(
        &self,
        workflow_id: &str,
        run_id: &str,
        node_id: &str,
        approval_policy_id: &str,
        summary_sha256: &str,
    ) -> Result<Option<bool>, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WorkflowStoreRecord {
    contract_version: u32,
    sequence: u64,
    workflow_id: String,
    definition: Option<WorkflowDefinition>,
    payload_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct HistoryStoreRecord {
    contract_version: u32,
    sequence: u64,
    run_id: String,
    history: WorkflowRunHistory,
    payload_sha256: String,
}

pub struct WorkflowService {
    root: PathBuf,
    gate: Mutex<()>,
    daemon_capabilities: Mutex<BTreeSet<DaemonCapability>>,
    capability_catalog: Mutex<WorkflowCapabilityCatalog>,
    node_executor: Arc<dyn WorkflowNodeExecutor>,
    clock: Arc<dyn WorkflowClock>,
    cancellations: Mutex<HashMap<String, ActiveWorkflowRun>>,
    trigger_registrar: Option<Arc<dyn PersistentWorkflowTriggerRegistrar>>,
    approval_broker: Option<Arc<dyn WorkflowHumanApprovalBroker>>,
    /// Sink for the unified process table, injected as a port rather than a
    /// ledger handle so this service stays storage-agnostic: its own history is
    /// a JSON file store, its unit tests must not need SQLite, and the same
    /// projection has to reach the desktop, the CLI, and daemon-triggered runs.
    /// `None` means "do not project" — a bare `monkey workflow` checkout and
    /// every unit test construct this service without a ledger, and a workflow
    /// must still run there.
    process_projector: Option<Arc<dyn ProcessProjector>>,
}

#[derive(Clone)]
struct ActiveWorkflowRun {
    workflow_id: String,
    cancellation: CancellationToken,
}

/// `WorkflowRunStatus` → a process projection.
///
/// `WorkflowRunStatus` has no queued or paused state — a run is `Running` from
/// construction — so there is nothing that maps to `Admitted` or `Suspended`.
fn workflow_run_projection(history: &WorkflowRunHistory) -> Option<ProcessProjection> {
    let (state, exit) = match &history.status {
        WorkflowRunStatus::Running => (ProcessState::Running, None),
        WorkflowRunStatus::Succeeded => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Succeeded,
                code: None,
                signal: None,
                reason: None,
            }),
        ),
        WorkflowRunStatus::Failed => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Failed,
                code: None,
                signal: None,
                reason: None,
            }),
        ),
        WorkflowRunStatus::Cancelled => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Cancelled,
                code: None,
                signal: None,
                reason: None,
            }),
        ),
        WorkflowRunStatus::NeedsReconciliation => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::NeedsReconciliation,
                code: None,
                signal: None,
                reason: Some("workflow run left effects that cannot be safely undone".to_string()),
            }),
        ),
    };

    let mut projection = ProcessProjection::new(
        ProcessKind::WorkflowRun,
        history.run_id.clone(),
        state,
    );
    projection.exit = exit;
    Some(projection)
}

/// A node instance's globally unique surface id.
///
/// `node_id` is authored in the workflow definition and is unique only *within*
/// that definition, so a node instance had no global identity at all. Qualifying
/// it with the run id gives it one without inventing a second id scheme.
fn workflow_node_external_id(run_id: &str, node_id: &str) -> String {
    format!("{run_id}:{node_id}")
}

fn workflow_node_projection(
    run_id: &str,
    node_id: &str,
    node: &NodeRunRecord,
) -> ProcessProjection {
    let (state, exit) = match &node.status {
        NodeRunStatus::Pending => (ProcessState::Admitted, None),
        NodeRunStatus::Running => (ProcessState::Running, None),
        NodeRunStatus::Succeeded => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Succeeded,
                code: None,
                signal: None,
                reason: None,
            }),
        ),
        NodeRunStatus::Failed { class, message } => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Failed,
                code: None,
                signal: None,
                reason: Some(format!("{class:?}: {message}")),
            }),
        ),
        NodeRunStatus::Skipped { reason } => (
            ProcessState::Exited,
            // A skipped node did not fail — it was never meant to run on this
            // path. `Succeeded` would claim it did work it did not do, so it
            // exits as cancelled with the reason it was skipped.
            Some(ProcessExit {
                status: ExitStatus::Cancelled,
                code: None,
                signal: None,
                reason: Some(format!("skipped: {reason}")),
            }),
        ),
        NodeRunStatus::NeedsReconciliation { receipt } => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::NeedsReconciliation,
                code: None,
                signal: None,
                reason: Some(format!("{receipt:?}")),
            }),
        ),
        NodeRunStatus::Reused { source_run_id } => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Succeeded,
                code: None,
                signal: None,
                reason: Some(format!("reused from run {source_run_id}")),
            }),
        ),
    };

    let mut projection = ProcessProjection::new(
        ProcessKind::WorkflowNode,
        workflow_node_external_id(run_id, node_id),
        state,
    )
    .with_parent(ProcessKind::WorkflowRun, run_id.to_string());
    projection.exit = exit;
    projection
}

impl WorkflowService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: impl AsRef<Path>,
        daemon_capabilities: BTreeSet<DaemonCapability>,
        capability_catalog: WorkflowCapabilityCatalog,
        node_executor: Arc<dyn WorkflowNodeExecutor>,
        clock: Arc<dyn WorkflowClock>,
        trigger_registrar: Option<Arc<dyn PersistentWorkflowTriggerRegistrar>>,
    ) -> Result<Self, M4ServiceError> {
        let root = root.as_ref();
        if root.exists() && fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(M4ServiceError::Io(
                "workflow service root cannot be a symlink".to_string(),
            ));
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        for child in ["workflows", "history"] {
            let path = root.join(child);
            if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(M4ServiceError::Io(format!(
                    "workflow service directory cannot be a symlink: {}",
                    path.display()
                )));
            }
            fs::create_dir_all(path)?;
        }
        Ok(Self {
            root,
            gate: Mutex::new(()),
            daemon_capabilities: Mutex::new(daemon_capabilities),
            capability_catalog: Mutex::new(capability_catalog),
            node_executor,
            clock,
            cancellations: Mutex::new(HashMap::new()),
            trigger_registrar,
            approval_broker: None,
            process_projector: None,
        })
    }

    /// Attaches the unified process table as a projection sink.
    ///
    /// Builder-style, matching how `approval_broker` is layered on after
    /// [`Self::new`], so the three existing construction sites keep working and
    /// a caller without a ledger simply does not call this.
    pub fn with_process_projector(mut self, projector: Arc<dyn ProcessProjector>) -> Self {
        self.process_projector = Some(projector);
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_approval_broker(
        root: impl AsRef<Path>,
        daemon_capabilities: BTreeSet<DaemonCapability>,
        capability_catalog: WorkflowCapabilityCatalog,
        node_executor: Arc<dyn WorkflowNodeExecutor>,
        clock: Arc<dyn WorkflowClock>,
        trigger_registrar: Option<Arc<dyn PersistentWorkflowTriggerRegistrar>>,
        approval_broker: Arc<dyn WorkflowHumanApprovalBroker>,
    ) -> Result<Self, M4ServiceError> {
        let mut service = Self::new(
            root,
            daemon_capabilities,
            capability_catalog,
            node_executor,
            clock,
            trigger_registrar,
        )?;
        service.approval_broker = Some(approval_broker);
        Ok(service)
    }

    pub fn set_runtime_capabilities(
        &self,
        daemon_capabilities: BTreeSet<DaemonCapability>,
        capability_catalog: WorkflowCapabilityCatalog,
    ) -> Result<(), M4ServiceError> {
        *lock(&self.daemon_capabilities, "daemon capabilities")? = daemon_capabilities;
        *lock(&self.capability_catalog, "workflow capability catalog")? = capability_catalog;
        Ok(())
    }

    pub fn validate(&self, definition: &WorkflowDefinition) -> Result<WorkflowIr, M4ServiceError> {
        let daemon = lock(&self.daemon_capabilities, "daemon capabilities")?.clone();
        let catalog = lock(&self.capability_catalog, "workflow capability catalog")?.clone();
        Ok(compile_workflow(definition, &daemon, &catalog)?)
    }

    pub fn prepare_human_approval(
        &self,
        workflow_id: &str,
        run_id: &str,
        node_id: &str,
        summary: &str,
    ) -> Result<WorkflowHumanApprovalChallenge, M4ServiceError> {
        if run_id.is_empty() || run_id.len() > 160 || summary.len() > 256 * 1024 {
            return Err(M4ServiceError::Workflow(
                "approval run id or summary is malformed".to_string(),
            ));
        }
        let definition = self.load(workflow_id)?;
        let _ = self.validate(&definition)?;
        let policy = definition
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .and_then(|node| match &node.kind {
                crate::workflow_core::WorkflowNodeKind::HumanApproval { approval_policy_id } => {
                    Some(approval_policy_id.as_str())
                }
                _ => None,
            })
            .ok_or_else(|| {
                M4ServiceError::NotFound(format!("workflow approval node {workflow_id}/{node_id}"))
            })?;
        self.approval_broker
            .as_ref()
            .ok_or_else(|| {
                M4ServiceError::Dependency("workflow approval broker is not configured".to_string())
            })?
            .prepare(
                workflow_id,
                run_id,
                node_id,
                policy,
                summary,
                &sha256(summary.as_bytes()),
            )
            .map_err(M4ServiceError::Dependency)
    }

    pub fn human_approval_challenge(
        &self,
        challenge_id: &str,
    ) -> Result<WorkflowHumanApprovalChallenge, M4ServiceError> {
        self.approval_broker
            .as_ref()
            .ok_or_else(|| {
                M4ServiceError::Dependency("workflow approval broker is not configured".to_string())
            })?
            .get(challenge_id)
            .map_err(M4ServiceError::Dependency)?
            .ok_or_else(|| {
                M4ServiceError::NotFound(format!(
                    "workflow approval challenge {challenge_id} is unknown or expired"
                ))
            })
    }

    pub fn decide_human_approval(
        &self,
        challenge_id: &str,
        approved: bool,
    ) -> Result<(), M4ServiceError> {
        self.approval_broker
            .as_ref()
            .ok_or_else(|| {
                M4ServiceError::Dependency("workflow approval broker is not configured".to_string())
            })?
            .decide(challenge_id, approved)
            .map_err(M4ServiceError::Dependency)
    }

    pub fn create(&self, definition: WorkflowDefinition) -> Result<WorkflowIr, M4ServiceError> {
        let ir = self.validate(&definition)?;
        let _guard = lock(&self.gate, "workflow store")?;
        if self
            .load_workflow_record_unlocked(&definition.workflow_id)?
            .is_some_and(|record| record.definition.is_some())
        {
            return Err(M4ServiceError::Conflict(format!(
                "workflow {} already exists",
                definition.workflow_id
            )));
        }
        let workflow_id = definition.workflow_id.clone();
        self.append_workflow_record_unlocked(&workflow_id, Some(definition))?;
        Ok(ir)
    }

    pub fn update(&self, definition: WorkflowDefinition) -> Result<WorkflowIr, M4ServiceError> {
        let ir = self.validate(&definition)?;
        let _guard = lock(&self.gate, "workflow store")?;
        let current = self
            .load_workflow_record_unlocked(&definition.workflow_id)?
            .and_then(|record| record.definition)
            .ok_or_else(|| {
                M4ServiceError::NotFound(format!("workflow {}", definition.workflow_id))
            })?;
        if definition.workflow_version <= current.workflow_version {
            return Err(M4ServiceError::Conflict(
                "workflow update version must increase".to_string(),
            ));
        }
        let workflow_id = definition.workflow_id.clone();
        self.append_workflow_record_unlocked(&workflow_id, Some(definition))?;
        Ok(ir)
    }

    pub fn import_legacy(&self, recipe: LegacyRecipeV1) -> Result<WorkflowIr, M4ServiceError> {
        self.create(adapt_legacy_recipe(recipe)?)
    }

    pub fn load(&self, workflow_id: &str) -> Result<WorkflowDefinition, M4ServiceError> {
        let _guard = lock(&self.gate, "workflow store")?;
        self.load_workflow_record_unlocked(workflow_id)?
            .and_then(|record| record.definition)
            .ok_or_else(|| M4ServiceError::NotFound(format!("workflow {workflow_id}")))
    }

    pub fn list(&self) -> Result<Vec<WorkflowDefinition>, M4ServiceError> {
        let _guard = lock(&self.gate, "workflow store")?;
        let mut workflows = Vec::new();
        for entry in fs::read_dir(self.root.join("workflows"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                continue;
            }
            if let Some(record) = self.load_latest_record::<WorkflowStoreRecord>(&entry.path())? {
                if self.validate_workflow_store_record(&record) {
                    if let Some(definition) = record.definition {
                        workflows.push(definition);
                    }
                }
            }
        }
        workflows.sort_by(|left, right| left.workflow_id.cmp(&right.workflow_id));
        Ok(workflows)
    }

    pub fn delete(&self, workflow_id: &str) -> Result<(), M4ServiceError> {
        if lock(&self.cancellations, "workflow cancellations")?
            .values()
            .any(|active| active.workflow_id == workflow_id)
        {
            return Err(M4ServiceError::Conflict(
                "cannot delete a workflow while one of its runs is active".to_string(),
            ));
        }
        let _guard = lock(&self.gate, "workflow store")?;
        if self
            .load_workflow_record_unlocked(workflow_id)?
            .and_then(|record| record.definition)
            .is_none()
        {
            return Err(M4ServiceError::NotFound(format!("workflow {workflow_id}")));
        }
        self.append_workflow_record_unlocked(workflow_id, None)?;
        Ok(())
    }

    pub fn replay(
        &self,
        workflow_id: &str,
        source_run_id: &str,
        boundary_node_id: &str,
        replay_approval_granted: bool,
        request: WorkflowRunRequest,
    ) -> Result<(ReplayPlan, WorkflowRunHistory), M4ServiceError> {
        let definition = self.load(workflow_id)?;
        let ir = self.validate(&definition)?;
        let source = self.history(source_run_id)?;
        let plan = plan_replay(&ir, &source, boundary_node_id, replay_approval_granted)?;
        self.ensure_new_run(&request.run_id)?;
        let cancel = CancellationToken::new();
        lock(&self.cancellations, "workflow cancellations")?.insert(
            request.run_id.clone(),
            ActiveWorkflowRun {
                workflow_id: workflow_id.to_string(),
                cancellation: cancel.clone(),
            },
        );
        let result = HeadlessWorkflowExecutor::new(
            self.node_executor.as_ref(),
            self.clock.as_ref(),
        )
        .replay(&ir, request.clone(), &source, &plan, &cancel);
        lock(&self.cancellations, "workflow cancellations")?.remove(&request.run_id);
        let history = result?;
        self.append_history(&history)?;
        Ok((plan, history))
    }

    pub fn cancel(&self, run_id: &str) -> Result<bool, M4ServiceError> {
        let cancellations = lock(&self.cancellations, "workflow cancellations")?;
        if let Some(active) = cancellations.get(run_id) {
            active.cancellation.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn history(&self, run_id: &str) -> Result<WorkflowRunHistory, M4ServiceError> {
        let _guard = lock(&self.gate, "workflow history")?;
        self.load_history_record_unlocked(run_id)?
            .map(|record| record.history)
            .ok_or_else(|| M4ServiceError::NotFound(format!("workflow run {run_id}")))
    }

    pub fn histories(&self) -> Result<Vec<WorkflowRunHistory>, M4ServiceError> {
        let _guard = lock(&self.gate, "workflow history")?;
        let mut histories = Vec::new();
        for entry in fs::read_dir(self.root.join("history"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                continue;
            }
            if let Some(record) = self.load_latest_record::<HistoryStoreRecord>(&entry.path())? {
                if self.validate_history_store_record(&record) {
                    histories.push(record.history);
                }
            }
        }
        histories.sort_by(|left, right| {
            right
                .started_unix_ms
                .cmp(&left.started_unix_ms)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        Ok(histories)
    }

    pub fn inspect_node(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<NodeRunRecord, M4ServiceError> {
        Ok(self.history(run_id)?.inspect_node(node_id)?.clone())
    }

    pub fn reconcile(
        &self,
        run_id: &str,
        node_id: &str,
        decision: ReconciliationDecision,
        now_unix_ms: u64,
    ) -> Result<WorkflowRunHistory, M4ServiceError> {
        let mut history = self.history(run_id)?;
        reconcile_node(&mut history, node_id, decision, now_unix_ms)?;
        self.append_history(&history)?;
        Ok(history)
    }

    pub fn register_persistent_triggers(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<String>, M4ServiceError> {
        let definition = self.load(workflow_id)?;
        let ir = self.validate(&definition)?;
        let triggers = ir
            .triggers
            .iter()
            .filter(|trigger| {
                matches!(
                    trigger,
                    WorkflowTrigger::PersistentCron { .. }
                        | WorkflowTrigger::Filesystem { .. }
                        | WorkflowTrigger::SignedWebhook { .. }
                        | WorkflowTrigger::EventIngestion { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if triggers.is_empty() {
            return Ok(Vec::new());
        }
        let registrar = self.trigger_registrar.as_ref().ok_or_else(|| {
            M4ServiceError::Dependency(
                "persistent triggers require the M6 daemon adapter".to_string(),
            )
        })?;
        registrar
            .replace_batch(&WorkflowTriggerBatch {
                contract_version: M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
                workflow_id: ir.workflow_id,
                workflow_version: ir.workflow_version,
                definition_sha256: ir.definition_sha256,
                triggers,
            })
            .map_err(M4ServiceError::Dependency)
    }

    pub fn unregister_persistent_triggers(&self, workflow_id: &str) -> Result<(), M4ServiceError> {
        self.trigger_registrar
            .as_ref()
            .ok_or_else(|| {
                M4ServiceError::Dependency(
                    "persistent triggers require the M6 daemon adapter".to_string(),
                )
            })?
            .remove_workflow(workflow_id)
            .map_err(M4ServiceError::Dependency)
    }

    pub fn run_workflow(
        &self,
        workflow_id: &str,
        request: WorkflowRunRequest,
    ) -> Result<WorkflowRunHistory, M4ServiceError> {
        let definition = self.load(workflow_id)?;
        let ir = self.validate(&definition)?;
        self.ensure_new_run(&request.run_id)?;
        let cancel = CancellationToken::new();
        lock(&self.cancellations, "workflow cancellations")?.insert(
            request.run_id.clone(),
            ActiveWorkflowRun {
                workflow_id: workflow_id.to_string(),
                cancellation: cancel.clone(),
            },
        );
        let result = HeadlessWorkflowExecutor::new(
            self.node_executor.as_ref(),
            self.clock.as_ref(),
        )
        .run(&ir, request.clone(), &cancel);
        lock(&self.cancellations, "workflow cancellations")?.remove(&request.run_id);
        let history = result?;
        self.append_history(&history)?;
        Ok(history)
    }

    fn ensure_new_run(&self, run_id: &str) -> Result<(), M4ServiceError> {
        if lock(&self.cancellations, "workflow cancellations")?.contains_key(run_id)
            || self.history_exists(run_id)?
        {
            return Err(M4ServiceError::Conflict(format!(
                "workflow run id {run_id} already exists"
            )));
        }
        Ok(())
    }

    fn history_exists(&self, run_id: &str) -> Result<bool, M4ServiceError> {
        let _guard = lock(&self.gate, "workflow history")?;
        Ok(self.load_history_record_unlocked(run_id)?.is_some())
    }

    fn append_history(&self, history: &WorkflowRunHistory) -> Result<(), M4ServiceError> {
        let _guard = lock(&self.gate, "workflow history")?;
        let previous = self.load_history_record_unlocked(&history.run_id)?;
        let sequence = previous.map_or(1, |record| record.sequence.saturating_add(1));
        let record = HistoryStoreRecord {
            contract_version: M4_SERVICE_CONTRACT_VERSION,
            sequence,
            run_id: history.run_id.clone(),
            history: history.clone(),
            payload_sha256: sha256(&serde_json::to_vec(history)?),
        };
        let directory = self
            .root
            .join("history")
            .join(sha256(history.run_id.as_bytes()));
        self.append_record(&directory, sequence, &record)?;
        self.project_processes(history);
        Ok(())
    }

    /// Projects a workflow run, and each of its node instances, onto the unified
    /// process table.
    ///
    /// Placed here because `append_history` is the single choke point every run
    /// state change flows through — the alternative, projecting at each call
    /// site, misses daemon-triggered runs, which reach this service directly
    /// rather than through `m4_commands.rs`.
    ///
    /// Fail-soft and deliberately not part of `append_history`'s `Result`: the
    /// workflow's own durable history has already been written by this point,
    /// and a projection failure must not turn a completed run into a reported
    /// error.
    fn project_processes(&self, history: &WorkflowRunHistory) {
        let Some(projector) = self.process_projector.as_ref() else {
            return;
        };

        let run_projection = match workflow_run_projection(history) {
            Some(projection) => projection,
            None => return,
        };
        if let Err(error) = projector.project(&run_projection) {
            eprintln!(
                "workflow service: could not project run {}: {error}",
                history.run_id
            );
            // Nodes hang off the run, so a failed run projection makes their
            // parent edge unresolvable. Stop rather than emit orphans.
            return;
        }

        for (node_id, node) in &history.nodes {
            let projection = workflow_node_projection(&history.run_id, node_id, node);
            if let Err(error) = projector.project(&projection) {
                eprintln!(
                    "workflow service: could not project node {}:{node_id}: {error}",
                    history.run_id
                );
            }
        }
    }

    fn load_history_record_unlocked(
        &self,
        run_id: &str,
    ) -> Result<Option<HistoryStoreRecord>, M4ServiceError> {
        let directory = self.root.join("history").join(sha256(run_id.as_bytes()));
        match self.load_latest_record::<HistoryStoreRecord>(&directory)? {
            Some(record)
                if record.run_id == run_id && self.validate_history_store_record(&record) =>
            {
                Ok(Some(record))
            }
            Some(_) => Err(M4ServiceError::Io(format!(
                "history record integrity check failed for {run_id}"
            ))),
            None => Ok(None),
        }
    }

    fn validate_history_store_record(&self, record: &HistoryStoreRecord) -> bool {
        record.contract_version == M4_SERVICE_CONTRACT_VERSION
            && record.sequence > 0
            && record.run_id == record.history.run_id
            && serde_json::to_vec(&record.history)
                .map(|bytes| sha256(&bytes) == record.payload_sha256)
                .unwrap_or(false)
    }

    fn append_workflow_record_unlocked(
        &self,
        workflow_id: &str,
        definition: Option<WorkflowDefinition>,
    ) -> Result<(), M4ServiceError> {
        let previous = self.load_workflow_record_unlocked(workflow_id)?;
        let sequence = previous.map_or(1, |record| record.sequence.saturating_add(1));
        let payload_sha256 = sha256(&serde_json::to_vec(&definition)?);
        let record = WorkflowStoreRecord {
            contract_version: M4_SERVICE_CONTRACT_VERSION,
            sequence,
            workflow_id: workflow_id.to_string(),
            definition,
            payload_sha256,
        };
        let directory = self
            .root
            .join("workflows")
            .join(sha256(workflow_id.as_bytes()));
        self.append_record(&directory, sequence, &record)
    }

    fn load_workflow_record_unlocked(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowStoreRecord>, M4ServiceError> {
        let directory = self
            .root
            .join("workflows")
            .join(sha256(workflow_id.as_bytes()));
        match self.load_latest_record::<WorkflowStoreRecord>(&directory)? {
            Some(record)
                if record.workflow_id == workflow_id
                    && self.validate_workflow_store_record(&record) =>
            {
                Ok(Some(record))
            }
            Some(_) => Err(M4ServiceError::Io(format!(
                "workflow record integrity check failed for {workflow_id}"
            ))),
            None => Ok(None),
        }
    }

    fn validate_workflow_store_record(&self, record: &WorkflowStoreRecord) -> bool {
        record.contract_version == M4_SERVICE_CONTRACT_VERSION
            && record.sequence > 0
            && record
                .definition
                .as_ref()
                .is_none_or(|definition| definition.workflow_id == record.workflow_id)
            && serde_json::to_vec(&record.definition)
                .map(|bytes| sha256(&bytes) == record.payload_sha256)
                .unwrap_or(false)
    }

    fn append_record<T: Serialize>(
        &self,
        directory: &Path,
        sequence: u64,
        record: &T,
    ) -> Result<(), M4ServiceError> {
        if directory.exists() && fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err(M4ServiceError::Io(
                "append-only record directory cannot be a symlink".to_string(),
            ));
        }
        fs::create_dir_all(directory)?;
        let path = directory.join(format!(
            "record-{sequence:020}-{}.json",
            Uuid::new_v4().simple()
        ));
        let bytes = serde_json::to_vec(record)?;
        let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(directory)?;
        Ok(())
    }

    fn load_latest_record<T: DeserializeOwned>(
        &self,
        directory: &Path,
    ) -> Result<Option<T>, M4ServiceError> {
        if !directory.exists() {
            return Ok(None);
        }
        if fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err(M4ServiceError::Io(
                "record directory cannot be a symlink".to_string(),
            ));
        }
        let mut entries = fs::read_dir(directory)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_type()
                    .is_ok_and(|kind| kind.is_file() && !kind.is_symlink())
                    && entry.file_name().to_string_lossy().starts_with("record-")
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        let Some(entry) = entries.pop() else {
            return Ok(None);
        };
        let bytes = fs::read(entry.path())?;
        Ok(Some(serde_json::from_slice::<T>(&bytes)?))
    }
}

fn sync_directory(path: &Path) -> Result<(), M4ServiceError> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_app_core::{
        DeclaredHostAction, HostActionKind, OAuthCodeExchangeRequest, OAuthRefreshRequest,
        OAuthTokenSet, PkceMaterial, SecretMaterial, SecretReference,
    };
    use crate::package_ecosystem::{
        RingEd25519SignatureVerifier, FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS, PACKAGE_STATE_VERSION,
    };
    use crate::workflow_core::{
        workflow_core_fixture_capabilities, workflow_core_fixtures, NodeAdapterResult,
        NodeExecutionRequest, ResourceUsage, WorkflowNodeKind, WorkflowRunStatus, WorkflowValue,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-m4-service-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn package_service(root: &Path) -> PackageRegistryService {
        let (trust_store, _, _) = signed_first_party_catalog().unwrap();
        PackageRegistryService::new(
            root,
            trust_store,
            InstallEnvironment {
                app_version: SemanticVersion::new(1, 0, 0),
                platform: "macos".to_string(),
                architecture: "aarch64".to_string(),
            },
            InstallTrustPolicy::default(),
            PackageLimits::default(),
            Arc::new(RingEd25519SignatureVerifier),
        )
        .unwrap()
    }

    #[test]
    fn package_service_seeds_previews_authorizes_and_completes_lifecycle() {
        let directory = TempDirectory::new("packages");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let seeded = service.seed_first_party(now).unwrap();
        assert_eq!(seeded.len(), 10);
        assert_eq!(service.seed_first_party(now).unwrap().len(), 10);
        assert_eq!(service.catalog(now).unwrap().len(), 10);
        let package = seeded.first().unwrap();
        let preview = service
            .preview(&package.manifest.package_id, package.manifest.version, now)
            .unwrap();
        let mut authorization = PackageInstallAuthorization {
            package_id: package.manifest.package_id.clone(),
            version: package.manifest.version,
            approval_digest: "wrong".to_string(),
            approved: true,
        };
        assert!(matches!(
            service.install(&authorization, now),
            Err(M4ServiceError::Conflict(_))
        ));
        authorization.approval_digest = preview.approval_digest;
        let installed = service.install(&authorization, now).unwrap();
        assert_eq!(installed.schema_version, PACKAGE_STATE_VERSION);
        assert!(installed.enabled);
        assert!(
            !service
                .set_enabled(&authorization.package_id, false)
                .unwrap()
                .enabled
        );
        assert_eq!(
            service.export(&authorization.package_id).unwrap().manifest,
            package.manifest
        );
        assert!(
            service
                .uninstall(&authorization.package_id)
                .unwrap()
                .tombstoned
        );
        assert!(service.installed().unwrap().is_empty());
    }

    #[test]
    fn uninstall_removes_plugin_from_installed_and_runtime_views() {
        let directory = TempDirectory::new("plugin-runtime-uninstall");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let package = service
            .seed_first_party(now)
            .unwrap()
            .into_iter()
            .find(|entry| entry.manifest.package_id == "com.littlemonkey.skill.review")
            .unwrap();
        let preview = service
            .preview(&package.manifest.package_id, package.manifest.version, now)
            .unwrap();
        service
            .install(
                &PackageInstallAuthorization {
                    package_id: package.manifest.package_id.clone(),
                    version: package.manifest.version,
                    approval_digest: preview.approval_digest,
                    approved: true,
                },
                now,
            )
            .unwrap();
        assert_eq!(service.installed().unwrap().len(), 1);
        assert_eq!(
            service
                .plugin_runtime(
                    &BTreeMap::new(),
                    &BTreeSet::new(),
                    &BTreeSet::new(),
                    &BTreeSet::new(),
                )
                .unwrap()
                .len(),
            1
        );

        service.uninstall(&package.manifest.package_id).unwrap();

        assert!(service.installed().unwrap().is_empty());
        assert!(service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap()
            .is_empty());

        // Tombstone metadata still exists internally so reinstall sequencing
        // and audit history remain intact; it is simply no longer a runtime.
        assert!(service
            .store
            .installed(&package.manifest.package_id)
            .unwrap()
            .is_some_and(|state| state.tombstoned));
    }

    #[test]
    fn plugin_runtime_reports_blocked_package_without_active_version() {
        let directory = TempDirectory::new("plugin-runtime-active-none");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let package = service
            .seed_first_party(now)
            .unwrap()
            .into_iter()
            .find(|entry| entry.manifest.package_id == "com.littlemonkey.skill.review")
            .unwrap();
        let preview = service
            .preview(&package.manifest.package_id, package.manifest.version, now)
            .unwrap();
        service
            .install(
                &PackageInstallAuthorization {
                    package_id: package.manifest.package_id.clone(),
                    version: package.manifest.version,
                    approval_digest: preview.approval_digest,
                    approved: true,
                },
                now,
            )
            .unwrap();

        // Simulate a state written by an older schema or foreign tool: not
        // tombstoned, yet without an active version. validate() accepts it,
        // so both views must agree on how it is reported.
        let mut state = service
            .store
            .installed(&package.manifest.package_id)
            .unwrap()
            .unwrap();
        state.active_version = None;
        state.sequence += 1;
        let state_directory = fs::read_dir(directory.0.join("state"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        fs::write(
            state_directory.join(format!("state-{:020}-manual.json", state.sequence)),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        let installed = service.installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert!(installed[0].active_version.is_none());

        let plugins = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].health, PluginRuntimeHealth::Blocked);
        assert!(plugins[0].version.is_none());
        assert!(!plugins[0].issues.is_empty());
    }

    #[test]
    fn enabled_skill_packages_are_runtime_discoverable_and_disable_immediately() {
        let directory = TempDirectory::new("active-skills");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let seeded = service.seed_first_party(now).unwrap();
        let package = seeded
            .iter()
            .find(|entry| entry.manifest.kind == PackageKind::Skill)
            .expect("first-party skill");
        let preview = service
            .preview(&package.manifest.package_id, package.manifest.version, now)
            .unwrap();
        service
            .install(
                &PackageInstallAuthorization {
                    package_id: package.manifest.package_id.clone(),
                    version: package.manifest.version,
                    approval_digest: preview.approval_digest,
                    approved: true,
                },
                now,
            )
            .unwrap();

        let active = service.active_skills().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].package_id, package.manifest.package_id);
        assert!(!active[0].instructions.is_empty());
        assert!(!active[0].content_sha256.is_empty());

        service
            .set_enabled(&package.manifest.package_id, false)
            .unwrap();
        assert!(service.active_skills().unwrap().is_empty());
    }

    #[test]
    fn plugin_runtime_reports_health_dependencies_and_verified_rollback_cache() {
        let directory = TempDirectory::new("plugin-runtime");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let seeded = service.seed_first_party(now).unwrap();
        for package in seeded.iter().filter(|entry| {
            entry.manifest.package_id == "com.littlemonkey.skill.review"
                || entry.manifest.package_id == "com.littlemonkey.connector.github"
        }) {
            let preview = service
                .preview(&package.manifest.package_id, package.manifest.version, now)
                .unwrap();
            service
                .install(
                    &PackageInstallAuthorization {
                        package_id: package.manifest.package_id.clone(),
                        version: package.manifest.version,
                        approval_digest: preview.approval_digest,
                        approved: true,
                    },
                    now,
                )
                .unwrap();
        }

        let runtime = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap();
        let skill = runtime
            .iter()
            .find(|plugin| plugin.package_id == "com.littlemonkey.skill.review")
            .unwrap();
        assert_eq!(skill.health, PluginRuntimeHealth::Healthy);
        assert!(skill.signed);
        assert!(skill.components.iter().any(|component| {
            component.kind == PluginComponentKind::Skill
                && component.state == PluginComponentState::Active
        }));
        let connector = runtime
            .iter()
            .find(|plugin| plugin.package_id == "com.littlemonkey.connector.github")
            .unwrap();
        assert_eq!(connector.health, PluginRuntimeHealth::NeedsSetup);
        let snapshots = service.active_plugin_snapshots().unwrap();
        let skill_snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.package_id == "com.littlemonkey.skill.review")
            .unwrap();
        assert!(skill_snapshot.text_content.contains_key("instructions.md"));

        let configured = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::from(["https://api.github.com".to_string()]),
                &BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(
            configured
                .iter()
                .find(|plugin| plugin.package_id == "com.littlemonkey.connector.github")
                .unwrap()
                .health,
            PluginRuntimeHealth::Healthy
        );

        service
            .set_enabled("com.littlemonkey.skill.review", false)
            .unwrap();
        let disabled = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(
            disabled
                .iter()
                .find(|plugin| plugin.package_id == "com.littlemonkey.skill.review")
                .unwrap()
                .health,
            PluginRuntimeHealth::Disabled
        );
        assert!(!service
            .active_plugin_snapshots()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.package_id == "com.littlemonkey.skill.review"));
    }

    #[test]
    fn portable_acquisition_requires_the_out_of_band_digest_pin() {
        let directory = TempDirectory::new("portable-import");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let seeded = service.seed_first_party(now).unwrap();
        let package = seeded
            .iter()
            .find(|entry| entry.manifest.package_id == "com.littlemonkey.skill.testing")
            .unwrap();
        let preview = service
            .preview(&package.manifest.package_id, package.manifest.version, now)
            .unwrap();
        service
            .install(
                &PackageInstallAuthorization {
                    package_id: package.manifest.package_id.clone(),
                    version: package.manifest.version,
                    approval_digest: preview.approval_digest,
                    approved: true,
                },
                now,
            )
            .unwrap();
        let portable = service.export(&package.manifest.package_id).unwrap();
        assert!(matches!(
            service.import_portable(portable.clone(), Some(&"0".repeat(64)), now),
            Err(M4ServiceError::Conflict(_))
        ));
        let expected = portable.bundle_sha256.clone();
        let imported = service
            .import_portable(portable, Some(&expected), now)
            .unwrap();
        assert_eq!(imported.bundle_sha256, expected);
    }

    #[test]
    fn plugin_workflow_templates_are_namespaced_and_runtime_visible() {
        let directory = TempDirectory::new("plugin-workflow");
        let service = package_service(&directory.0);
        let now = FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS;
        let mut bundle = crate::package_ecosystem::first_party_package_fixtures()
            .into_iter()
            .next()
            .unwrap()
            .bundle;
        let workflow = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        let path = "workflows/review.json".to_string();
        let bytes = serde_json::to_vec(&workflow).unwrap();
        let digest = sha256(&bytes);
        let persona_path = "persona.md".to_string();
        let persona_bytes = b"You are a careful review assistant.".to_vec();
        let persona_digest = sha256(&persona_bytes);
        let rule_path = "rules/review.md".to_string();
        let rule_bytes = b"Cite every actionable finding.".to_vec();
        let rule_digest = sha256(&rule_bytes);
        bundle.manifest.package_id = "com.example.plugin.workflow".to_string();
        bundle.manifest.kind = PackageKind::Assistant;
        bundle.manifest.display_name = "Workflow plugin".to_string();
        bundle.manifest.content = vec![
            crate::package_ecosystem::ContentReference {
                kind: ContentKind::Persona,
                path: persona_path.clone(),
                media_type: "text/markdown".to_string(),
                sha256: persona_digest.clone(),
            },
            crate::package_ecosystem::ContentReference {
                kind: ContentKind::WorkflowTemplate,
                path: path.clone(),
                media_type: "application/json".to_string(),
                sha256: digest.clone(),
            },
            crate::package_ecosystem::ContentReference {
                kind: ContentKind::Rule,
                path: rule_path.clone(),
                media_type: "text/markdown".to_string(),
                sha256: rule_digest.clone(),
            },
        ];
        bundle.manifest.assistant = Some(crate::package_ecosystem::AssistantComposition {
            persona_content_path: persona_path.clone(),
            skill_package_ids: BTreeSet::new(),
            starter_workflow_paths: vec![path.clone()],
            knowledge_template_path: None,
        });
        bundle.manifest.file_checksums = BTreeMap::from([
            (path.clone(), digest),
            (persona_path.clone(), persona_digest),
            (rule_path.clone(), rule_digest),
        ]);
        bundle.manifest.model_requirements.clear();
        bundle.files = BTreeMap::from([
            (path.clone(), bytes),
            (persona_path, persona_bytes),
            (rule_path, rule_bytes),
        ]);
        service.import_bundle(bundle, now).unwrap();
        let preview = service
            .preview(
                "com.example.plugin.workflow",
                SemanticVersion::new(1, 0, 0),
                now,
            )
            .unwrap();
        service
            .install(
                &PackageInstallAuthorization {
                    package_id: "com.example.plugin.workflow".to_string(),
                    version: SemanticVersion::new(1, 0, 0),
                    approval_digest: preview.approval_digest,
                    approved: true,
                },
                now,
            )
            .unwrap();

        let template = service
            .plugin_workflow_template("com.example.plugin.workflow", &path)
            .unwrap();
        let workflow_id = plugin_workflow_id("com.example.plugin.workflow", &path);
        assert_eq!(template.workflow_id, workflow_id);
        assert!(template
            .name
            .ends_with(&plugin_workflow_marker("com.example.plugin.workflow")));
        let available = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(available[0].health, PluginRuntimeHealth::NeedsSetup);
        assert!(available[0].components.iter().any(|component| {
            component.kind == PluginComponentKind::Assistant
                && component.state == PluginComponentState::NeedsSetup
        }));
        assert!(available[0].components.iter().any(|component| {
            component.kind == PluginComponentKind::Workflow
                && component.state == PluginComponentState::Available
        }));
        let active = service
            .plugin_runtime(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::from([workflow_id]),
            )
            .unwrap();
        assert_eq!(active[0].health, PluginRuntimeHealth::Healthy);
        assert!(active[0].components.iter().any(|component| {
            component.kind == PluginComponentKind::Assistant
                && component.state == PluginComponentState::Active
        }));
        assert!(active[0].components.iter().any(|component| {
            component.kind == PluginComponentKind::Rule
                && component.state == PluginComponentState::Active
        }));
        assert!(active[0].components.iter().any(|component| {
            component.kind == PluginComponentKind::Workflow
                && component.state == PluginComponentState::Active
        }));
    }

    struct InertSecurity;

    impl OAuthSecurityProvider for InertSecurity {
        fn generate_pkce(&self) -> Result<PkceMaterial, String> {
            Err("not configured in UI-only test".to_string())
        }
    }

    struct InertVault;

    impl OAuthSecretVault for InertVault {
        fn put_ephemeral(
            &self,
            _label: &str,
            _secret: SecretMaterial,
        ) -> Result<SecretReference, String> {
            Err("unused".to_string())
        }
        fn get_ephemeral(&self, _reference: &SecretReference) -> Result<SecretMaterial, String> {
            Err("unused".to_string())
        }
        fn delete_ephemeral(&self, _reference: &SecretReference) -> Result<(), String> {
            Err("unused".to_string())
        }
        fn put_tokens(
            &self,
            _server_id: &str,
            _tokens: OAuthTokenSet,
        ) -> Result<SecretReference, String> {
            Err("unused".to_string())
        }
        fn get_tokens(&self, _reference: &SecretReference) -> Result<OAuthTokenSet, String> {
            Err("unused".to_string())
        }
        fn replace_tokens(
            &self,
            _reference: &SecretReference,
            _tokens: OAuthTokenSet,
        ) -> Result<(), String> {
            Err("unused".to_string())
        }
        fn delete_tokens(&self, _reference: &SecretReference) -> Result<(), String> {
            Err("unused".to_string())
        }
    }

    struct InertFlows;

    impl OAuthFlowStore for InertFlows {
        fn put(&self, _state: crate::mcp_app_core::PendingOAuthFlow) -> Result<(), String> {
            Err("unused".to_string())
        }
        fn take_by_state_hash(
            &self,
            _state_sha256: &str,
        ) -> Result<Option<crate::mcp_app_core::PendingOAuthFlow>, String> {
            Err("unused".to_string())
        }
    }

    struct InertTransport;

    impl OAuthTransport for InertTransport {
        fn exchange_code(
            &self,
            _request: OAuthCodeExchangeRequest,
        ) -> Result<OAuthTokenSet, String> {
            Err("unused".to_string())
        }
        fn refresh(&self, _request: OAuthRefreshRequest) -> Result<OAuthTokenSet, String> {
            Err("unused".to_string())
        }
        fn revoke(
            &self,
            _endpoint: &str,
            _client_id: &str,
            _token: SecretMaterial,
        ) -> Result<(), String> {
            Err("unused".to_string())
        }
    }

    struct FixedUiIssuer;

    impl McpUiSessionIssuer for FixedUiIssuer {
        fn issue(&self) -> Result<(String, BridgeCapability), String> {
            Ok((
                "ui-session-1".to_string(),
                BridgeCapability::new(
                    "capability_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                )
                .unwrap(),
            ))
        }
    }

    struct AllowUi;

    impl UiActionApprovalGate for AllowUi {
        fn approve(
            &self,
            _session_id: &str,
            _action: &DeclaredHostAction,
            _payload_summary_sha256: &str,
        ) -> Result<Option<String>, String> {
            Ok(Some("approval-service-1".to_string()))
        }
    }

    impl UiActionApprovalBroker for AllowUi {
        fn prepare(
            &self,
            action: &PreparedBridgeAction,
        ) -> Result<UiActionApprovalChallenge, String> {
            Ok(UiActionApprovalChallenge {
                challenge_id: "challenge-service-1".to_string(),
                session_id: action.session_id.clone(),
                action_id: action.action.action_id.clone(),
                action_target: action.action.target.clone(),
                required_permission: action.action.required_permission.clone(),
                payload_summary_sha256: action.payload_summary_sha256.clone(),
            })
        }

        fn decide(&self, _challenge_id: &str, _approved: bool) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn mcp_ui_service_keeps_capability_session_bound_and_closes_it() {
        let service = McpAppService::new(
            Arc::new(InertSecurity),
            Arc::new(InertVault),
            Arc::new(InertFlows),
            Arc::new(InertTransport),
            Arc::new(FixedUiIssuer),
            Arc::new(AllowUi),
        );
        let bytes = b"<html></html>";
        let manifest = McpUiManifest {
            contract_version: crate::mcp_app_core::MCP_UI_HOST_CONTRACT_VERSION,
            server_id: "fixture".to_string(),
            resource_uri: "ui://fixture/panel".to_string(),
            resource_sha256: sha256(bytes),
            entry_media_type: "text/html".to_string(),
            network_origins: BTreeSet::new(),
            host_actions: BTreeMap::from([(
                "search".to_string(),
                DeclaredHostAction {
                    action_id: "search".to_string(),
                    kind: HostActionKind::InvokeTool,
                    target: "mcp__fixture__search".to_string(),
                    required_permission: "read".to_string(),
                    always_requires_approval: true,
                },
            )]),
            text_fallback: "Use text search".to_string(),
        };
        let opened = service
            .open_ui_session(
                manifest.clone(),
                bytes,
                BTreeSet::from(["read".to_string()]),
            )
            .unwrap();
        assert!(!opened.host_plan.tauri_ipc_exposed);
        let authorized = service
            .authorize_ui_action(
                &opened.session_id,
                opened.bridge_capability,
                UiBridgeRequest {
                    session_id: opened.session_id.clone(),
                    server_id: manifest.server_id,
                    resource_sha256: manifest.resource_sha256,
                    action_id: "search".to_string(),
                    payload: serde_json::json!({"query": "rust"}),
                },
            )
            .unwrap();
        assert_eq!(authorized.approval_id, "approval-service-1");
        assert!(service.close_ui_session(&opened.session_id).unwrap());
        assert!(service
            .authorize_ui_action(
                &opened.session_id,
                "capability_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                UiBridgeRequest {
                    session_id: opened.session_id.clone(),
                    server_id: "fixture".to_string(),
                    resource_sha256: sha256(bytes),
                    action_id: "search".to_string(),
                    payload: serde_json::json!({}),
                },
            )
            .is_err());
    }

    #[test]
    fn mcp_oauth_registration_metadata_survives_service_restart() {
        let directory = TempDirectory::new("mcp-oauth-state");
        let registration = McpOAuthServerRegistration {
            server: OAuthServerMetadata {
                contract_version: crate::mcp_app_core::MCP_OAUTH_CONTRACT_VERSION,
                issuer: "https://auth.example.com/".to_string(),
                authorization_endpoint: "https://auth.example.com/authorize".to_string(),
                token_endpoint: "https://auth.example.com/token".to_string(),
                revocation_endpoint: Some("https://auth.example.com/revoke".to_string()),
                supported_scopes: BTreeSet::from(["read".to_string()]),
                supports_pkce_s256: true,
            },
            client: OAuthClientConfig {
                server_id: "persistent-server".to_string(),
                client_id: "desktop-client".to_string(),
                redirect_uri: "littlemonkey://oauth/callback".to_string(),
                requested_scopes: BTreeSet::from(["read".to_string()]),
            },
        };
        let make_service = || {
            McpAppService::new_persistent(
                directory.0.join("state"),
                Arc::new(InertSecurity),
                Arc::new(InertVault),
                Arc::new(InertFlows),
                Arc::new(InertTransport),
                Arc::new(FixedUiIssuer),
                Arc::new(AllowUi),
            )
            .unwrap()
        };
        let service = make_service();
        service
            .register_oauth_server(registration.clone())
            .expect("persist registration");
        drop(service);
        let restored = make_service();
        assert_eq!(
            restored.oauth_servers().unwrap(),
            vec![registration.clone()]
        );
        assert!(matches!(
            restored.register_oauth_server(registration),
            Err(M4ServiceError::Conflict(_))
        ));
    }

    #[derive(Default)]
    struct ServiceClock(AtomicU64);

    impl WorkflowClock for ServiceClock {
        fn now_unix_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }

        fn sleep_ms(&self, duration_ms: u64, cancel: &CancellationToken) -> Result<(), String> {
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            self.0.fetch_add(duration_ms, Ordering::SeqCst);
            Ok(())
        }
    }

    struct IdentityExecutor;

    impl WorkflowNodeExecutor for IdentityExecutor {
        fn execute(
            &self,
            request: NodeExecutionRequest,
            _cancel: &CancellationToken,
        ) -> Result<NodeAdapterResult, String> {
            let output = match request.node.kind {
                WorkflowNodeKind::Transform { .. } | WorkflowNodeKind::Verify { .. } => {
                    request.inputs["input"].clone()
                }
                _ => return Err("test executor only supports transforms".to_string()),
            };
            Ok(NodeAdapterResult::Succeeded {
                outputs: BTreeMap::from([("out".to_string(), output)]),
                usage: ResourceUsage::default(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingTriggers {
        batches: Mutex<Vec<WorkflowTriggerBatch>>,
    }

    impl PersistentWorkflowTriggerRegistrar for RecordingTriggers {
        fn replace_batch(&self, batch: &WorkflowTriggerBatch) -> Result<Vec<String>, String> {
            self.batches.lock().unwrap().push(batch.clone());
            Ok(vec!["daemon-trigger-1".to_string()])
        }

        fn remove_workflow(&self, _workflow_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn workflow_service(
        root: &Path,
        registrar: Option<Arc<dyn PersistentWorkflowTriggerRegistrar>>,
        daemon: BTreeSet<DaemonCapability>,
    ) -> WorkflowService {
        WorkflowService::new(
            root,
            daemon,
            workflow_core_fixture_capabilities(),
            Arc::new(IdentityExecutor),
            Arc::new(ServiceClock::default()),
            registrar,
        )
        .unwrap()
    }

    /// Records projections instead of writing them, which is the whole reason
    /// `WorkflowService` takes a port rather than a ledger: this test needs no
    /// SQLite, no migrations, and no temp database.
    #[derive(Default)]
    struct RecordingProjector {
        seen: Mutex<Vec<ProcessProjection>>,
    }

    impl RecordingProjector {
        fn of_kind(&self, kind: ProcessKind) -> Vec<ProcessProjection> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|projection| projection.kind == kind)
                .cloned()
                .collect()
        }
    }

    impl ProcessProjector for RecordingProjector {
        fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
            self.seen.lock().unwrap().push(projection.clone());
            Ok(())
        }
    }

    struct FailingProjector;

    impl ProcessProjector for FailingProjector {
        fn project(&self, _projection: &ProcessProjection) -> Result<(), String> {
            Err("ledger is unavailable".to_string())
        }
    }

    #[test]
    fn a_workflow_run_and_every_node_are_projected_onto_the_process_table() {
        let directory = TempDirectory::new("workflow-projection");
        let projector = Arc::new(RecordingProjector::default());
        let service = workflow_service(&directory.0, None, BTreeSet::new())
            .with_process_projector(projector.clone());

        let definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        service.create(definition.clone()).unwrap();
        let history = service
            .run_workflow(
                &definition.workflow_id,
                WorkflowRunRequest {
                    run_id: "projected-run-1".to_string(),
                    inputs: BTreeMap::new(),
                    secret_bindings: BTreeMap::new(),
                    trigger: WorkflowTrigger::Manual,
                },
            )
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);

        let runs = projector.of_kind(ProcessKind::WorkflowRun);
        assert_eq!(runs.len(), 1, "the run is projected exactly once per append");
        assert_eq!(runs[0].external_id, "projected-run-1");
        assert_eq!(runs[0].state, ProcessState::Exited);
        assert_eq!(
            runs[0].exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Succeeded)
        );

        let nodes = projector.of_kind(ProcessKind::WorkflowNode);
        assert_eq!(nodes.len(), history.nodes.len());
        for node in &nodes {
            // A node id is unique only within its definition, so the surface id
            // must be run-qualified or two runs of the same workflow would
            // collide on one record.
            assert!(
                node.external_id.starts_with("projected-run-1:"),
                "node surface id is not run-qualified: {}",
                node.external_id
            );
            assert_eq!(
                node.parent,
                Some((ProcessKind::WorkflowRun, "projected-run-1".to_string())),
                "every node must name its run as parent"
            );
            assert!(node.exit.is_some(), "a finished node must carry an exit");
        }
    }

    #[test]
    fn two_runs_of_the_same_workflow_do_not_collide_on_one_node_record() {
        let directory = TempDirectory::new("workflow-projection-distinct");
        let projector = Arc::new(RecordingProjector::default());
        let service = workflow_service(&directory.0, None, BTreeSet::new())
            .with_process_projector(projector.clone());

        let definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        service.create(definition.clone()).unwrap();
        for run_id in ["distinct-a", "distinct-b"] {
            service
                .run_workflow(
                    &definition.workflow_id,
                    WorkflowRunRequest {
                        run_id: run_id.to_string(),
                        inputs: BTreeMap::new(),
                        secret_bindings: BTreeMap::new(),
                        trigger: WorkflowTrigger::Manual,
                    },
                )
                .unwrap();
        }

        let ids: BTreeSet<String> = projector
            .of_kind(ProcessKind::WorkflowNode)
            .into_iter()
            .map(|projection| projection.external_id)
            .collect();
        let total = projector.of_kind(ProcessKind::WorkflowNode).len();
        assert_eq!(
            ids.len(),
            total,
            "node surface ids collided across two runs: {ids:?}"
        );
    }

    #[test]
    fn a_workflow_run_still_succeeds_when_the_projection_cannot_be_written() {
        // The run's own durable history is written before the projection is
        // attempted, so a projection failure must never turn a completed run
        // into a reported error.
        let directory = TempDirectory::new("workflow-projection-fails");
        let service = workflow_service(&directory.0, None, BTreeSet::new())
            .with_process_projector(Arc::new(FailingProjector));

        let definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        service.create(definition.clone()).unwrap();
        let history = service
            .run_workflow(
                &definition.workflow_id,
                WorkflowRunRequest {
                    run_id: "projection-fails".to_string(),
                    inputs: BTreeMap::new(),
                    secret_bindings: BTreeMap::new(),
                    trigger: WorkflowTrigger::Manual,
                },
            )
            .expect("a projection failure must not fail the run");
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
        // And the durable history is intact.
        assert_eq!(
            service.history("projection-fails").unwrap().status,
            WorkflowRunStatus::Succeeded
        );
    }

    #[test]
    fn a_workflow_service_without_a_projector_runs_normally() {
        // The CLI and every other unit test construct this service with no
        // ledger; a workflow must still run there.
        let directory = TempDirectory::new("workflow-no-projector");
        let service = workflow_service(&directory.0, None, BTreeSet::new());
        let definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        service.create(definition.clone()).unwrap();
        let history = service
            .run_workflow(
                &definition.workflow_id,
                WorkflowRunRequest {
                    run_id: "no-projector".to_string(),
                    inputs: BTreeMap::new(),
                    secret_bindings: BTreeMap::new(),
                    trigger: WorkflowTrigger::Manual,
                },
            )
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
    }

    #[test]
    fn workflow_service_persists_crud_runs_replay_history_and_trigger_batches() {
        let directory = TempDirectory::new("workflows");
        let registrar = Arc::new(RecordingTriggers::default());
        let service = workflow_service(
            &directory.0,
            Some(registrar.clone()),
            BTreeSet::from([DaemonCapability::PersistentCron]),
        );
        let mut definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        definition.triggers.push(WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".to_string(),
        });
        let ir = service.create(definition.clone()).unwrap();
        assert_eq!(service.list().unwrap().len(), 1);
        assert_eq!(service.load(&definition.workflow_id).unwrap(), definition);
        assert_eq!(
            service
                .register_persistent_triggers(&definition.workflow_id)
                .unwrap(),
            vec!["daemon-trigger-1".to_string()]
        );
        assert_eq!(
            registrar.batches.lock().unwrap()[0].definition_sha256,
            ir.definition_sha256
        );

        let request = WorkflowRunRequest {
            run_id: "service-run-1".to_string(),
            inputs: BTreeMap::new(),
            secret_bindings: BTreeMap::new(),
            trigger: WorkflowTrigger::Manual,
        };
        let history = service
            .run_workflow(&definition.workflow_id, request)
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
        assert_eq!(
            service
                .inspect_node("service-run-1", "left")
                .unwrap()
                .outputs["out"],
            WorkflowValue::Json(serde_json::json!({"side": "left"}))
        );
        let (plan, replayed) = service
            .replay(
                &definition.workflow_id,
                "service-run-1",
                "left",
                false,
                WorkflowRunRequest {
                    run_id: "service-run-2".to_string(),
                    inputs: BTreeMap::new(),
                    secret_bindings: BTreeMap::new(),
                    trigger: WorkflowTrigger::Manual,
                },
            )
            .unwrap();
        assert!(plan.reused_node_ids.contains("right"));
        assert_eq!(replayed.status, WorkflowRunStatus::Succeeded);
        assert_eq!(service.histories().unwrap().len(), 2);

        definition.workflow_version = 2;
        service.update(definition.clone()).unwrap();
        assert_eq!(
            service
                .load(&definition.workflow_id)
                .unwrap()
                .workflow_version,
            2
        );
        drop(service);

        let reopened = workflow_service(
            &directory.0,
            Some(registrar),
            BTreeSet::from([DaemonCapability::PersistentCron]),
        );
        assert_eq!(
            reopened.history("service-run-1").unwrap().status,
            WorkflowRunStatus::Succeeded
        );
        reopened.delete(&definition.workflow_id).unwrap();
        assert!(reopened.load(&definition.workflow_id).is_err());
    }
}
