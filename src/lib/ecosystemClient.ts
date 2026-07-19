import { invoke } from "@tauri-apps/api/core";

export type SemanticVersion = string;

export type PackageKind = "skill" | "assistant" | "connector" | "collection";
export type PermissionKind =
  | "read_files"
  | "write_files"
  | "network"
  | "invoke_mcp_tool"
  | "use_model"
  | "create_artifact"
  | "execute_process"
  | "install_executable"
  | "read_raw_keychain";

export interface PackagePermission {
  permission_id: string;
  kind: PermissionKind | string;
  scope: string;
  reason: string;
}

export type PackageContentKind =
  | "instructions"
  | "prompt"
  | "persona"
  | "rule"
  | "workflow_template"
  | "knowledge_template"
  | "ui_resource";

export interface PackageContentReference {
  kind: PackageContentKind;
  path: string;
  media_type: string;
  sha256: string;
}

export type VulnerabilitySeverity = "low" | "medium" | "high" | "critical";

/** Manifest-declared only — there is no live CVE/vulnerability feed. */
export interface VulnerabilityNotice {
  notice_id: string;
  severity: VulnerabilitySeverity;
  summary: string;
  affected_versions: SemanticVersion[];
  advisory_url: string | null;
}

export interface PackageAssistantComposition {
  persona_content_path: string;
  skill_package_ids: string[];
  starter_workflow_paths: string[];
  knowledge_template_path: string | null;
}

export interface PackageManifest {
  schema_version: number;
  package_id: string;
  version: SemanticVersion;
  kind: PackageKind;
  display_name: string;
  description: string;
  content: PackageContentReference[];
  assistant?: PackageAssistantComposition | null;
  permissions: PackagePermission[];
  vulnerability_notices?: VulnerabilityNotice[];
  mcp_requirements: unknown[];
  provenance: {
    publisher: string;
    source: Record<string, unknown>;
    source_revision: string;
    build_reproducible: boolean;
  };
  [key: string]: unknown;
}

export interface TrustEvidence {
  signed: boolean;
  trust_root_id: string | null;
  key_id: string | null;
  registry_snapshot_sha256: string | null;
  revocation: Record<string, unknown>;
}

export interface PackageCatalogEntry {
  manifest: PackageManifest;
  bundle_sha256: string;
  trust: TrustEvidence | null;
  available: boolean;
  validation_error: string | null;
}

export interface PermissionDiff {
  added: PackagePermission[];
  removed: PackagePermission[];
  unchanged: PackagePermission[];
  approval_digest: string;
  requires_new_approval: boolean;
}

export interface InstallPreview {
  package_id: string;
  version: SemanticVersion;
  kind: PackageKind;
  source: Record<string, unknown>;
  bundle_sha256: string;
  trust: TrustEvidence;
  permissions: PackagePermission[];
  permission_diff: PermissionDiff | null;
  mcp_actions_separate: unknown[];
  file_count: number;
  total_bytes: number;
  warnings: string[];
}

export interface ApprovedInstallPreview {
  preview: InstallPreview;
  approval_digest: string;
}

export interface CachedVersion {
  version: SemanticVersion;
  bundle_sha256: string;
  trust: TrustEvidence;
}

export interface InstalledPackageState {
  schema_version: number;
  sequence: number;
  package_id: string;
  active_version: SemanticVersion | null;
  versions: Record<SemanticVersion, CachedVersion>;
  activation_history: SemanticVersion[];
  pinned_version: SemanticVersion | null;
  enabled: boolean;
  revoked: boolean;
  tombstoned: boolean;
  approved_permissions: PackagePermission[];
  /** Local-only counter; there is no hosted install telemetry in this app. */
  local_install_count: number;
  /** Locally user-set flag, independent of any role/permission system. */
  team_approved: boolean;
}

export interface ActiveSkillDescriptor {
  package_id: string;
  version: SemanticVersion;
  name: string;
  command: string;
  description: string;
  instructions: string;
  content_sha256: string;
  permissions: PackagePermission[];
}

export type PluginRuntimeHealth = "healthy" | "needs_setup" | "disabled" | "blocked" | "corrupt";
export type PluginComponentKind =
  | "skill"
  | "assistant"
  | "connector"
  | "instructions"
  | "prompt"
  | "persona"
  | "rule"
  | "workflow"
  | "knowledge_template"
  | "ui_resource"
  | "mcp_requirement";
export type PluginComponentState = "active" | "available" | "needs_setup" | "disabled" | "blocked";

export interface PluginComponentDescriptor {
  component_id: string;
  kind: PluginComponentKind;
  label: string;
  source_path: string | null;
  content_sha256: string | null;
  activation_id: string | null;
  state: PluginComponentState;
  detail: string;
}

export interface PluginRuntimeDescriptor {
  package_id: string;
  version: SemanticVersion | null;
  name: string;
  description: string;
  kind: PackageKind;
  health: PluginRuntimeHealth;
  enabled: boolean;
  signed: boolean;
  bundle_sha256: string | null;
  pinned_version: SemanticVersion | null;
  rollback_target: SemanticVersion | null;
  rollback_healthy: boolean;
  permissions: PackagePermission[];
  components: PluginComponentDescriptor[];
  issues: string[];
}

export interface ActivePluginRuntimeSnapshot {
  package_id: string;
  version: SemanticVersion;
  bundle_sha256: string;
  manifest: PackageManifest;
  text_content: Record<string, string>;
}

export interface PortablePackageExport {
  schema_version: number;
  bundle_sha256: string;
  manifest: PackageManifest;
  files_hex: Record<string, string>;
}

export interface RegistrySnapshot {
  schema_version: number;
  registry_id: string;
  sequence: number;
  generated_unix_ms: number;
  refresh_after_unix_ms: number;
  expires_unix_ms: number;
  packages: Record<string, unknown[]>;
  revocations: unknown[];
  signature: { trust_root_id: string; key_id: string; algorithm: string; signature_hex: string };
}

export interface VerifiedRegistryState {
  snapshot: RegistrySnapshot;
  verified_unix_ms: number;
  snapshot_sha256: string;
}

/** The roadmap's "private/team catalog": a user-added registry source. */
export interface AdditionalRegistrySource {
  source_id: string;
  display_name: string;
  location: string;
  added_unix_ms: number;
}

export interface AdditionalRegistryRecord {
  source: AdditionalRegistrySource;
  verified: VerifiedRegistryState | null;
  last_verification_error: string | null;
}

export interface OAuthServerMetadata {
  contract_version: number;
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
  revocation_endpoint: string | null;
  supported_scopes: string[];
  supports_pkce_s256: boolean;
}

export interface OAuthClientConfig {
  server_id: string;
  client_id: string;
  redirect_uri: string;
  requested_scopes: string[];
}

export interface McpOAuthServerRegistration {
  server: OAuthServerMetadata;
  client: OAuthClientConfig;
}

export interface OAuthAuthorizationPlan {
  flow_id: string;
  authorization_url: string;
  expires_unix_ms: number;
}

export interface SecretReference {
  vault_id: string;
  reference_id: string;
}

export interface OAuthTokenMetadata {
  token_reference: SecretReference;
  token_type: string;
  granted_scopes: string[];
  issued_unix_ms: number;
  expires_unix_ms: number;
}

export type HostActionKind =
  | "invoke_tool"
  | "open_external_url"
  | "write_clipboard_text"
  | "publish_artifact";

export interface DeclaredHostAction {
  action_id: string;
  kind: HostActionKind;
  target: string;
  required_permission: string;
  always_requires_approval: boolean;
}

export interface McpUiManifest {
  contract_version: number;
  server_id: string;
  resource_uri: string;
  resource_sha256: string;
  entry_media_type: "text/html" | "image/svg+xml";
  network_origins: string[];
  host_actions: Record<string, DeclaredHostAction>;
  text_fallback: string;
}

export interface McpUiHostPlan {
  opaque_origin_required: boolean;
  iframe_sandbox_tokens: string[];
  content_security_policy: string;
  bridge_action_ids: string[];
  tauri_ipc_exposed: boolean;
  filesystem_exposed: boolean;
  keychain_exposed: boolean;
  text_fallback: string;
}

export interface OpenedMcpUiSession {
  session_id: string;
  bridge_capability: string;
  host_plan: McpUiHostPlan;
}

export interface UiBridgeRequest {
  session_id: string;
  server_id: string;
  resource_sha256: string;
  action_id: string;
  payload: unknown;
}

export interface UiActionApprovalChallenge {
  challenge_id: string;
  session_id: string;
  action_id: string;
  action_target: string;
  required_permission: string;
  payload_summary_sha256: string;
}

export interface AuthorizedBridgeAction {
  session_id: string;
  action: DeclaredHostAction;
  payload: unknown;
  approval_id: string;
}

export type WorkflowValueType =
  | { kind: "string" }
  | { kind: "integer" }
  | { kind: "decimal" }
  | { kind: "boolean" }
  | { kind: "json" }
  | { kind: "artifact" }
  | { kind: "unit" }
  | { kind: "array"; item: WorkflowValueType };

export type WorkflowValue =
  | { kind: "string"; value: string }
  | { kind: "integer"; value: number }
  | { kind: "decimal"; value: number }
  | { kind: "boolean"; value: boolean }
  | { kind: "json"; value: unknown }
  | { kind: "artifact"; value: { artifact_id: string; sha256: string; media_type: string } }
  | { kind: "unit" }
  | { kind: "array"; value: WorkflowValue[] };

export type InputBinding =
  | { source: "workflow_input"; input_id: string }
  | { source: "node_output"; node_id: string; port: string }
  | { source: "literal"; value: WorkflowValue };

export type EffectClass = "pure" | "read_only" | "local_mutation" | "external_mutation";

export type WorkflowNodeKind =
  | { kind: "prompt_model"; model_selector: string }
  | { kind: "agent"; agent_profile: string; effect: EffectClass }
  | { kind: "subagent"; agent_profile: string; effect: EffectClass }
  | { kind: "tool"; tool_id: string; effect: EffectClass }
  | { kind: "mcp"; server_id: string; tool_name: string; effect: EffectClass }
  | { kind: "browser"; action: string; effect: EffectClass }
  | { kind: "git"; action: string; effect: EffectClass }
  | { kind: "pull_request"; action: string; effect: EffectClass }
  | { kind: "shell"; shell_profile: string }
  | { kind: "verify"; verifier_id: string }
  | { kind: "transform"; transform_id: string }
  | { kind: "condition" }
  | { kind: "bounded_loop"; maximum_iterations: number }
  | { kind: "human_approval"; approval_policy_id: string }
  | { kind: "artifact"; media_type: string }
  | { kind: "output" }
  | { kind: "legacy_recipe"; recipe: LegacyRecipeV1 };

export interface WorkflowNode {
  node_id: string;
  kind: WorkflowNodeKind;
  inputs: Record<string, InputBinding>;
  secret_ids: string[];
  permission_policy: {
    permission_ids: string[];
    approval_node_id: string | null;
  };
  retry: {
    maximum_attempts: number;
    initial_backoff_ms: number;
    maximum_backoff_ms: number;
    retry_on: string[];
  };
  timeout_ms: number;
  estimate: {
    model_calls: number;
    input_tokens: number;
    output_tokens: number;
    cost_microunits: number;
  };
  idempotency: Record<string, unknown>;
  replay: "safe" | "requires_approval" | "never";
  guard: { condition_node_id: string; expected: boolean } | null;
}

export type WorkflowTrigger =
  | { kind: "manual" }
  | { kind: "in_app_cron"; expression: string }
  | { kind: "persistent_cron"; expression: string }
  | { kind: "filesystem"; canonical_root: string; pattern: string }
  | { kind: "signed_webhook"; webhook_id: string; secret_reference: string; replay_window_ms: number }
  | { kind: "event_ingestion"; topic: string; consumer_id: string };

export interface WorkflowDefinition {
  schema_version: number;
  workflow_id: string;
  workflow_version: number;
  name: string;
  inputs: Record<string, WorkflowValueType>;
  secrets: Record<string, { secret_id: string; purpose: string; allowed_node_ids: string[] }>;
  nodes: WorkflowNode[];
  outputs: Record<string, { value_type: WorkflowValueType; binding: InputBinding }>;
  budgets: {
    maximum_node_executions: number;
    maximum_model_calls: number;
    maximum_input_tokens: number;
    maximum_output_tokens: number;
    maximum_cost_microunits: number;
    maximum_wall_time_ms: number;
  };
  maximum_concurrency: number;
  triggers: WorkflowTrigger[];
}

export interface WorkflowIrNode {
  node: WorkflowNode;
  dependencies: string[];
  level: number;
  input_types: Record<string, WorkflowValueType>;
  output_types: Record<string, WorkflowValueType>;
}

export interface WorkflowIr {
  ir_version: number;
  workflow_id: string;
  workflow_version: number;
  definition_sha256: string;
  inputs: Record<string, WorkflowValueType>;
  nodes: WorkflowIrNode[];
  triggers: WorkflowTrigger[];
  [key: string]: unknown;
}

export interface SecretBinding {
  secret_id: string;
  vault_reference: string;
}

export interface WorkflowRunRequest {
  run_id: string;
  inputs: Record<string, WorkflowValue>;
  secret_bindings: Record<string, SecretBinding>;
  trigger: WorkflowTrigger;
}

export interface NodeRunRecord {
  node_id: string;
  status: { status: string; [key: string]: unknown };
  inputs: Record<string, WorkflowValue>;
  secret_references: Record<string, SecretBinding>;
  outputs: Record<string, WorkflowValue>;
  pending_outputs: Record<string, WorkflowValue>;
  attempts: number;
  started_unix_ms: number | null;
  finished_unix_ms: number | null;
  usage: Record<string, number>;
}

export interface WorkflowRunHistory {
  schema_version: number;
  run_id: string;
  workflow_id: string;
  definition_sha256: string;
  status: "running" | "succeeded" | "failed" | "cancelled" | "needs_reconciliation";
  started_unix_ms: number;
  finished_unix_ms: number | null;
  trigger: WorkflowTrigger;
  input_snapshot: Record<string, WorkflowValue>;
  secret_reference_snapshot: Record<string, SecretBinding>;
  nodes: Record<string, NodeRunRecord>;
  outputs: Record<string, WorkflowValue>;
  usage: Record<string, number>;
  events: unknown[];
}

export interface LegacyRecipeV1 {
  version: number;
  name: string;
  target: { provider: string | null; model: string | null; ollama: string | null; local_url: string | null };
  permission_mode: string;
  system: string | null;
  prompt: string;
  params: Record<string, string | null>;
  maximum_iterations: number | null;
  timeout_seconds: number | null;
}

export interface WorkflowHumanApprovalChallenge {
  challenge_id: string;
  workflow_id: string;
  run_id: string;
  node_id: string;
  approval_policy_id: string;
  summary_sha256: string;
}

export interface WorkflowReplayResponse {
  plan: Record<string, unknown>;
  history: WorkflowRunHistory;
}

export const ecosystemClient = {
  seedPackages: (nowUnixMs = Date.now()) =>
    invoke<PackageCatalogEntry[]>("m4_packages_seed_first_party", { nowUnixMs }),
  packageCatalog: (nowUnixMs = Date.now()) =>
    invoke<PackageCatalogEntry[]>("m4_packages_catalog", { nowUnixMs }),
  installedPackages: () => invoke<InstalledPackageState[]>("m4_packages_installed"),
  activeSkills: () => invoke<ActiveSkillDescriptor[]>("m4_packages_active_skills"),
  activePluginSnapshots: () => invoke<ActivePluginRuntimeSnapshot[]>("m4_plugins_active_snapshot"),
  pluginRuntime: () => invoke<PluginRuntimeDescriptor[]>("m4_plugins_runtime"),
  importPortablePackage: (
    portable: PortablePackageExport,
    expectedBundleSha256: string | null = portable.bundle_sha256,
    nowUnixMs = Date.now(),
  ) => invoke<PackageCatalogEntry>("m4_packages_import_portable", {
    portable,
    expectedBundleSha256,
    nowUnixMs,
  }),
  previewPackage: (packageId: string, version: SemanticVersion, nowUnixMs = Date.now()) =>
    invoke<ApprovedInstallPreview>("m4_packages_preview", { packageId, version, nowUnixMs }),
  installPackage: (authorization: {
    package_id: string;
    version: SemanticVersion;
    approval_digest: string;
    approved: boolean;
  }, nowUnixMs = Date.now()) =>
    invoke<InstalledPackageState>("m4_packages_install", { authorization, nowUnixMs }),
  updatePackage: (
    packageId: string,
    version: SemanticVersion,
    approval: {
      package_id: string;
      from_version: SemanticVersion;
      to_version: SemanticVersion;
      approval_digest: string;
      approved: boolean;
    } | null,
    nowUnixMs = Date.now(),
  ) => invoke<InstalledPackageState>("m4_packages_update", { packageId, version, approval, nowUnixMs }),
  setPackageEnabled: (packageId: string, enabled: boolean) =>
    invoke<InstalledPackageState>("m4_packages_set_enabled", { packageId, enabled }),
  pinPackage: (packageId: string, version: SemanticVersion | null) =>
    invoke<InstalledPackageState>("m4_packages_pin", { packageId, version }),
  rollbackPackage: (packageId: string) =>
    invoke<InstalledPackageState>("m4_packages_rollback", { packageId }),
  uninstallPackage: (packageId: string) =>
    invoke<InstalledPackageState>("m4_packages_uninstall", { packageId }),
  exportPackage: (packageId: string) =>
    invoke<PortablePackageExport>("m4_packages_export", { packageId }),
  setPackageTeamApproved: (packageId: string, teamApproved: boolean) =>
    invoke<InstalledPackageState>("m4_packages_set_team_approved", { packageId, teamApproved }),

  listRegistrySources: () => invoke<AdditionalRegistryRecord[]>("m4_registries_list"),
  addRegistrySource: (
    sourceId: string,
    displayName: string,
    location: string,
    nowUnixMs = Date.now(),
  ) => invoke<AdditionalRegistryRecord>("m4_registries_add", {
    sourceId,
    displayName,
    location,
    nowUnixMs,
  }),
  removeRegistrySource: (sourceId: string) =>
    invoke<boolean>("m4_registries_remove", { sourceId }),
  verifyRegistrySource: (sourceId: string, snapshot: RegistrySnapshot, nowUnixMs = Date.now()) =>
    invoke<AdditionalRegistryRecord>("m4_registries_verify", { sourceId, snapshot, nowUnixMs }),
  activatePluginWorkflow: (packageId: string, contentPath: string) =>
    invoke<WorkflowIr>("m4_plugins_activate_workflow", { packageId, contentPath }),
  deactivatePluginWorkflow: (packageId: string, contentPath: string) =>
    invoke<boolean>("m4_plugins_deactivate_workflow", { packageId, contentPath }),

  registerOAuth: (registration: McpOAuthServerRegistration) =>
    invoke<void>("m4_mcp_oauth_register", { registration }),
  oauthServers: () => invoke<McpOAuthServerRegistration[]>("m4_mcp_oauth_servers"),
  beginOAuth: (serverId: string, lifetimeMs = 10 * 60_000, nowUnixMs = Date.now()) =>
    invoke<OAuthAuthorizationPlan>("m4_mcp_oauth_begin", { serverId, nowUnixMs, lifetimeMs }),
  completeOAuth: (serverId: string, state: string, code: string, nowUnixMs = Date.now()) =>
    invoke<OAuthTokenMetadata>("m4_mcp_oauth_complete", {
      serverId,
      callback: { state, code, error: null },
      nowUnixMs,
    }),
  refreshOAuth: (serverId: string, nowUnixMs = Date.now()) =>
    invoke<OAuthTokenMetadata>("m4_mcp_oauth_refresh", { serverId, nowUnixMs }),
  revokeOAuth: (serverId: string) => invoke<void>("m4_mcp_oauth_revoke", { serverId }),
  oauthMetadata: (serverId: string) =>
    invoke<OAuthTokenMetadata | null>("m4_mcp_oauth_metadata", { serverId }),

  openMcpUi: (manifest: McpUiManifest, resourceBytes: number[], grantedPermissions: string[]) =>
    invoke<OpenedMcpUiSession>("m4_mcp_ui_open", { manifest, resourceBytes, grantedPermissions }),
  prepareMcpUiAction: (sessionId: string, presentedCapability: string, request: UiBridgeRequest) =>
    invoke<UiActionApprovalChallenge>("m4_mcp_ui_prepare_action", {
      sessionId,
      presentedCapability,
      request,
    }),
  decideMcpUiAction: (challengeId: string, approved: boolean) =>
    invoke<void>("m4_mcp_ui_decide_action", { challengeId, approved }),
  authorizeMcpUiAction: (sessionId: string, presentedCapability: string, request: UiBridgeRequest) =>
    invoke<AuthorizedBridgeAction>("m4_mcp_ui_authorize_action", {
      sessionId,
      presentedCapability,
      request,
    }),
  closeMcpUi: (sessionId: string) => invoke<boolean>("m4_mcp_ui_close", { sessionId }),

  workflows: () => invoke<WorkflowDefinition[]>("m4_workflows_list"),
  loadWorkflow: (workflowId: string) =>
    invoke<WorkflowDefinition>("m4_workflows_load", { workflowId }),
  validateWorkflow: (definition: WorkflowDefinition) =>
    invoke<WorkflowIr>("m4_workflows_validate", { definition }),
  refreshWorkflowCapabilities: () => invoke<void>("m4_workflows_refresh_capabilities"),
  createWorkflow: (definition: WorkflowDefinition) =>
    invoke<WorkflowIr>("m4_workflows_create", { definition }),
  updateWorkflow: (definition: WorkflowDefinition) =>
    invoke<WorkflowIr>("m4_workflows_update", { definition }),
  importLegacyWorkflow: (recipe: LegacyRecipeV1) =>
    invoke<WorkflowIr>("m4_workflows_import_legacy", { recipe }),
  deleteWorkflow: (workflowId: string) => invoke<void>("m4_workflows_delete", { workflowId }),
  runWorkflow: (workflowId: string, request: WorkflowRunRequest) =>
    invoke<WorkflowRunHistory>("m4_workflows_run", { workflowId, request }),
  cancelWorkflow: (runId: string) => invoke<boolean>("m4_workflows_cancel", { runId }),
  prepareWorkflowApproval: (workflowId: string, runId: string, nodeId: string, summary: string) =>
    invoke<WorkflowHumanApprovalChallenge>("m4_workflows_prepare_approval", {
      workflowId,
      runId,
      nodeId,
      summary,
    }),
  decideWorkflowApproval: (challengeId: string, approved: boolean) =>
    invoke<void>("m4_workflows_decide_approval", { challengeId, approved }),
  replayWorkflow: (
    workflowId: string,
    sourceRunId: string,
    boundaryNodeId: string,
    replayApprovalGranted: boolean,
    request: WorkflowRunRequest,
  ) => invoke<WorkflowReplayResponse>("m4_workflows_replay", {
    workflowId,
    sourceRunId,
    boundaryNodeId,
    replayApprovalGranted,
    request,
  }),
  workflowHistories: () => invoke<WorkflowRunHistory[]>("m4_workflows_histories"),
  workflowHistory: (runId: string) =>
    invoke<WorkflowRunHistory>("m4_workflows_history", { runId }),
  inspectWorkflowNode: (runId: string, nodeId: string) =>
    invoke<NodeRunRecord>("m4_workflows_inspect_node", { runId, nodeId }),
  reconcileWorkflowNode: (
    runId: string,
    nodeId: string,
    decision: "verified_applied" | "verified_not_applied" | "abandon",
    nowUnixMs = Date.now(),
  ) => invoke<WorkflowRunHistory>("m4_workflows_reconcile", {
    runId,
    nodeId,
    decision,
    nowUnixMs,
  }),
  registerWorkflowTriggers: (workflowId: string) =>
    invoke<string[]>("m4_workflows_register_triggers", { workflowId }),
  unregisterWorkflowTriggers: (workflowId: string) =>
    invoke<void>("m4_workflows_unregister_triggers", { workflowId }),
};
