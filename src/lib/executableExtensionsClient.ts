import { invoke } from "@tauri-apps/api/core";

export type CapabilityKind =
  | "tool"
  | "channel"
  | "model_provider"
  | "embedding_provider"
  | "stt"
  | "tts"
  | "realtime_voice"
  | "web_search"
  | "web_fetch"
  | "device_provider"
  | "connector";

export type PermissionKind =
  | "network_origin"
  | "workspace_read"
  | "workspace_write"
  | "artifact_read"
  | "artifact_write"
  | "model_invoke"
  | "secret_use"
  | "device"
  | "webhook_receive";

export type PermissionRisk = "low" | "medium" | "high" | "critical";
export type TrustState = "verified" | "unsigned" | "untrusted" | "invalid";
export type HealthState =
  | "not_validated"
  | "stopped"
  | "healthy"
  | "degraded"
  | "unhealthy"
  | "protective_disabled";

export interface VersionConstraint {
  minimum: string;
  maximum_exclusive?: string | null;
}

export interface Compatibility {
  minimum_app_version: string;
  maximum_app_version_exclusive: string | null;
  platforms: string[];
  architectures: string[];
  contract?: VersionConstraint | null;
}

export type InstallSource =
  | { local_folder: { canonical_path: string } }
  | { git: { remote: string; commit_sha: string } }
  | { curated_registry: { registry_id: string } };

export interface Provenance {
  publisher: string;
  source: InstallSource;
  source_revision: string;
  build_reproducible: boolean;
}

export interface Signature {
  trust_root_id: string;
  key_id: string;
  algorithm: string;
  signature_hex: string;
}

export interface CapabilityDeclaration {
  capability_id: string;
  kind: CapabilityKind;
  display_name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

export interface ActiveCapability {
  kind: CapabilityKind;
  capability_id: string;
  extension_id: string;
  version: string;
  display_name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

export interface PermissionDeclaration {
  permission_id: string;
  kind: PermissionKind;
  scope: string;
  reason: string;
}

export interface ConfigField {
  key: string;
  label: string;
  description: string;
  kind: "string" | "integer" | "boolean" | "select";
  required: boolean;
  default: unknown | null;
  options: string[];
  minimum: number | null;
  maximum: number | null;
}

export interface SecretSlot {
  slot_id: string;
  label: string;
  description: string;
  auth_header: string | null;
  auth_scheme: string | null;
}

export interface ExtensionManifest {
  schema_version: number;
  extension_id: string;
  version: string;
  display_name: string;
  description: string;
  host_api: VersionConstraint;
  component: { path: string; sha256: string };
  capabilities: CapabilityDeclaration[];
  permissions: PermissionDeclaration[];
  config_schema: ConfigField[];
  secret_slots: SecretSlot[];
  dependencies: { extension_id: string; constraint: VersionConstraint }[];
  compatibility: Compatibility;
  publisher: string;
  provenance: Provenance;
  signature: Signature | null;
  checksums: Record<string, string>;
}

export interface TrustEvidence {
  state: TrustState;
  reason: string;
  trust_root_id: string | null;
  key_id: string | null;
  manifest_sha256: string;
  component_sha256: string;
}

export interface PermissionView {
  permission_id: string;
  kind: PermissionKind;
  scope: string;
  reason: string;
  risk: PermissionRisk;
  granted: boolean;
  binding_label: string | null;
}

export interface PermissionDiff {
  added: PermissionView[];
  removed: PermissionView[];
  unchanged: PermissionView[];
  expands_authority: boolean;
}

export interface RuntimeHealth {
  state: HealthState;
  validated: boolean;
  enabled: boolean;
  running: boolean;
  consecutive_failures: number;
  trap_count: number;
  undeclared_attempts: number;
  last_error: string | null;
  last_invocation_at_ms: number | null;
}

export interface SecretSlotStatus {
  slot_id: string;
  label: string;
  description: string;
  configured: boolean;
}

export interface ExtensionPreview {
  source_path: string;
  source_digest: string;
  manifest: ExtensionManifest;
  trust: TrustEvidence;
  compatible: boolean;
  compatibility_reason: string | null;
  permissions: PermissionView[];
  permission_diff: PermissionDiff | null;
  approval_digest: string;
  requires_unsigned_approval: boolean;
  requires_untrusted_approval: boolean;
  requires_high_risk_approval: boolean;
  blockers: string[];
}

export interface ExtensionDetail {
  manifest: ExtensionManifest;
  trust: TrustEvidence;
  installed_source: InstallSource;
  compatible: boolean;
  compatibility_reason: string | null;
  permissions: PermissionView[];
  secret_slots: SecretSlotStatus[];
  config: Record<string, unknown>;
  health: RuntimeHealth;
  active_version: string;
  previous_version: string | null;
  available_versions: string[];
  update_available: boolean;
  allowed_actions: string[];
  blockers: string[];
}

export interface PermissionGrant {
  permission_id: string;
  binding: string | null;
}

export interface ExtensionApproval {
  approval_digest: string;
  grants: PermissionGrant[];
  allow_unsigned: boolean;
  allow_untrusted: boolean;
  allow_high_risk: boolean;
}

export interface ExtensionLogRow {
  at_ms: number;
  level: string;
  message: string;
  invocation_id: string | null;
}

export interface InvocationRequest {
  extension_id: string;
  capability_id: string;
  input_json: string;
  invocation_id: string | null;
  input_artifact_ids: string[];
  expected_kind: CapabilityKind | null;
  expected_version: string | null;
}

export interface InvocationResult {
  invocation_id: string;
  output_json: string;
  duration_ms: number;
  fuel_consumed: number;
  emitted_events: [string, string][];
  tool_result: string | null;
}

export interface ExtensionWebhookStatus {
  trigger_id: string;
  handler_id: string;
  version: string;
  enabled: boolean;
}

export const executableExtensionsClient = {
  discover: (sourcePath: string) =>
    invoke<ExtensionPreview>("extensions_discover", { sourcePath }),
  list: () => invoke<ExtensionDetail[]>("extensions_list"),
  activeCapabilities: (kind?: CapabilityKind) =>
    invoke<ActiveCapability[]>("extensions_active_capabilities", { kind: kind ?? null }),
  inspect: (extensionId: string) =>
    invoke<ExtensionDetail>("extensions_inspect", { extensionId }),
  install: (sourcePath: string, approval: ExtensionApproval) =>
    invoke<ExtensionDetail>("extensions_install", { sourcePath, approval }),
  validate: (extensionId: string) =>
    invoke<ExtensionDetail>("extensions_validate", { extensionId }),
  setEnabled: (extensionId: string, enabled: boolean) =>
    invoke<ExtensionDetail>("extensions_set_enabled", { extensionId, enabled }),
  setRunning: (extensionId: string, running: boolean) =>
    invoke<ExtensionDetail>("extensions_set_running", { extensionId, running }),
  previewUpdate: (sourcePath: string) =>
    invoke<ExtensionPreview>("extensions_preview_update", { sourcePath }),
  update: (sourcePath: string, approval: ExtensionApproval) =>
    invoke<ExtensionDetail>("extensions_update", { sourcePath, approval }),
  rollback: (extensionId: string) =>
    invoke<ExtensionDetail>("extensions_rollback", { extensionId }),
  uninstall: (extensionId: string) =>
    invoke<void>("extensions_uninstall", { extensionId }),
  status: (extensionId: string) =>
    invoke<ExtensionDetail>("extensions_status", { extensionId }),
  logs: (extensionId: string, limit = 100) =>
    invoke<ExtensionLogRow[]>("extensions_logs", { extensionId, limit }),
  setConfig: (extensionId: string, values: Record<string, unknown>) =>
    invoke<ExtensionDetail>("extensions_set_config", { extensionId, values }),
  setSecret: (extensionId: string, slotId: string, secret: string) =>
    invoke<void>("extensions_set_secret", { extensionId, slotId, secret }),
  removeSecret: (extensionId: string, slotId: string) =>
    invoke<void>("extensions_remove_secret", { extensionId, slotId }),
  invoke: (request: InvocationRequest) =>
    invoke<InvocationResult>("extensions_invoke", { request }),
  cancel: (invocationId: string) =>
    invoke<boolean>("extensions_cancel", { invocationId }),
  webhooks: (extensionId: string) =>
    invoke<ExtensionWebhookStatus[]>("extensions_webhooks", { extensionId }),
  registerWebhook: (
    triggerId: string,
    extensionId: string,
    handlerId: string,
    secret: string,
    maxSkewMs = 300_000,
  ) => invoke<ExtensionWebhookStatus[]>("extensions_register_webhook", {
    triggerId,
    extensionId,
    handlerId,
    secret,
    maxSkewMs,
  }),
  removeWebhook: (triggerId: string, extensionId: string) =>
    invoke<ExtensionWebhookStatus[]>("extensions_remove_webhook", {
      triggerId,
      extensionId,
    }),
};
