import { invoke } from "@tauri-apps/api/core";

export type M3RuntimeKind = "ollama" | "llama_cpp" | "mlx";
export type AcceleratorKind = "cpu" | "metal" | "cuda" | "rocm" | "vulkan" | "direct_ml";
export type HardwareTier = "constrained" | "balanced" | "performance";
export type ApiBackend = "managed_local" | "ollama" | "mlx" | "cloud_provider";
export type ApiScope =
  | "chat_completions"
  | "responses"
  | "messages"
  | "model_discover"
  | "model_download"
  | "model_load"
  | "model_unload"
  | "model_delete"
  | "model_status";
export type CompatibilityProtocol =
  | "open_ai_chat_completions"
  | "open_ai_responses"
  | "anthropic_messages";

export interface AcceleratorCapability {
  kind: AcceleratorKind;
  available: boolean;
  device_names: string[];
  total_memory_bytes: number | null;
  available_memory_bytes: number | null;
}

export interface PlatformCapabilities {
  os: string;
  arch: string;
  supported_runtimes: Array<"ollama" | "llama_cpp">;
  accelerators: AcceleratorCapability[];
}

export interface HardwareSnapshot {
  captured_at_ms: number;
  total_ram_bytes: number;
  available_ram_bytes: number;
  logical_cpu_count: number;
  platform: PlatformCapabilities;
}

export interface HardwareProfile {
  tier: HardwareTier;
  recommended_process_slots: number;
  recommended_ram_reserve_bytes: number;
  preferred_accelerator: AcceleratorKind;
}

/** Hardware Compatibility Matrix / "Driver Doctor" status for one backend. */
export type M3AcceleratorStatus =
  | "available"
  | "not_detected"
  | "driver_too_old"
  | "tool_missing"
  | "unsupported";

export interface M3AcceleratorCompatibility {
  kind: AcceleratorKind;
  status: M3AcceleratorStatus;
  summary: string;
  deviceNames: string[];
  driverVersion: string | null;
  computeCapability: string | null;
  confirmed: boolean;
}

export interface M3JetsonInfo {
  detected: boolean;
  model: string | null;
}

export interface M3HardwareCompatibilityReport {
  capturedAtMs: number;
  os: string;
  arch: string;
  accelerators: M3AcceleratorCompatibility[];
  jetson: M3JetsonInfo;
  hybridGraphicsDetected: boolean;
  notes: string[];
}

export interface M3StorageStatus {
  root: string;
  quotaBytes: number;
  reserveBytes: number;
  usedBytes: number;
  availableForModelsBytes: number;
  pendingDownloadBytes: number;
}

export interface M3ModelCapabilities {
  chat: boolean;
  embeddings: boolean;
  toolCalling: boolean;
  vision: boolean;
  structuredOutput: boolean;
}

/** Coarse chat-template family the Chat Template Compatibility Lab groups
 * fixtures by — see `chat_template_lab.rs`'s module doc comment for why
 * detection is deliberately this coarse. */
export type TemplateFamily = "chatml" | "llama3" | "mistral" | "gemma" | "generic";

/** One fixture area from the ROADMAP wording ("tool rendering, image
 * blocks, thinking modes, system prompts, and stop tokens"), plus
 * structured output. */
export type CapabilityArea =
  | "tool_calling"
  | "system_prompt"
  | "stop_token"
  | "structured_output"
  | "vision"
  | "thinking";

export interface ChatTemplateLabResult {
  area: CapabilityArea;
  passed: boolean;
  detail: string;
}

export interface ChatTemplateLabReport {
  templateFamily: TemplateFamily;
  results: ChatTemplateLabResult[];
}

/** Mirrors `chat_template_lab.rs`'s `gate_capabilities`: a capability can
 * only stay `true` if it was already declared true AND the lab actually
 * verified it for this template family. `embeddings` has no chat-template
 * fixture and passes through unchanged. */
export function gateCapabilities(
  capabilities: M3ModelCapabilities,
  report: ChatTemplateLabReport | undefined,
): M3ModelCapabilities {
  if (!report) return capabilities;
  const passed = (area: CapabilityArea) =>
    report.results.some((result) => result.area === area && result.passed);
  return {
    chat: capabilities.chat && passed("system_prompt") && passed("stop_token"),
    embeddings: capabilities.embeddings,
    toolCalling: capabilities.toolCalling && passed("tool_calling"),
    vision: capabilities.vision && passed("vision"),
    structuredOutput: capabilities.structuredOutput && passed("structured_output"),
  };
}

export interface M3ModelLicense {
  name: string;
  spdxId: string | null;
  sourceUrl: string;
  revision: string;
  retrievedAtMs: number;
  rawDeclaration: string;
}

export interface M3ProjectorRef {
  kind: string;
  sha256: string;
  sizeBytes: number;
}

export interface M3CatalogModel {
  schemaVersion: number;
  sourceId: string;
  modelId: string;
  displayName: string;
  runtime: M3RuntimeKind;
  variantId: string;
  revision: string;
  quantization: string | null;
  downloadUrl: string;
  sha256: string;
  sizeBytes: number;
  estimatedRamBytes: number;
  estimatedVramBytes: number;
  supportedOs: string[];
  supportedArch: string[];
  requiredAccelerator: string | null;
  capabilities: M3ModelCapabilities;
  license: M3ModelLicense;
  metadata: Record<string, string>;
  template: string | null;
  projector: M3ProjectorRef | null;
  catalogRetrievedAtMs: number | null;
}

export interface M3HardwareFit {
  rating: "recommended" | "tight" | "too_large" | "incompatible";
  requiredRamBytes: number;
  availableRamBytes: number;
  requiredVramBytes: number;
  availableVramBytes: number;
  reasons: string[];
}

export interface M3CatalogMatch {
  model: M3CatalogModel;
  fit: M3HardwareFit;
}

export interface M3InstalledVersion {
  versionKey: string;
  revision: string;
  sha256: string;
  sizeBytes: number;
  artifactPath: string;
  installedAtMs: number;
  active: boolean;
  license: M3ModelLicense;
  sourceId: string;
  template: string | null;
  projector: M3ProjectorRef | null;
  catalogRetrievedAtMs: number | null;
}

export interface M3InstalledModel {
  assetId: string;
  modelId: string;
  displayName: string;
  runtime: M3RuntimeKind;
  variantId: string;
  capabilities: M3ModelCapabilities;
  estimatedRamBytes: number;
  estimatedVramBytes: number;
  requiredAccelerator: string | null;
  activeVersionKey: string;
  versions: M3InstalledVersion[];
}

export interface M3CatalogSourceConfig {
  sourceId: string;
  endpoint: string;
}

export interface M3CleanupReport {
  removedPaths: number;
  reclaimedBytes: number;
}

// Runtime Component Update Channels: versioned `llama.cpp`/MLX/tokenizer/
// converter/projector/accelerator-support components, distinct from models
// above. See `M3ComponentHub` in `src-tauri/src/m3_runtime_hub.rs`.
export type M3ComponentKind =
  | "llama_cpp_server"
  | "mlx_runtime"
  | "tokenizer"
  | "converter"
  | "projector_runtime"
  | "metal_support"
  | "cuda_support"
  | "rocm_support"
  | "vulkan_support";

export type M3ComponentChannel = "stable" | "beta" | "pinned";

export interface M3ComponentCatalogEntry {
  schemaVersion: number;
  sourceId: string;
  componentId: string;
  kind: M3ComponentKind;
  displayName: string;
  accelerator: AcceleratorKind | null;
  version: string;
  channel: M3ComponentChannel;
  downloadUrl: string;
  sha256: string;
  sizeBytes: number;
  publishedAtMs: number;
  compatibilityNote: string | null;
  metadata: Record<string, string>;
}

export interface M3InstalledComponentVersion {
  versionKey: string;
  version: string;
  channel: M3ComponentChannel;
  sha256: string;
  sizeBytes: number;
  sourceUrl: string;
  artifactPath: string;
  installedAtMs: number;
  publishedAtMs: number;
  active: boolean;
  compatibilityNote: string | null;
}

export interface M3InstalledComponent {
  componentId: string;
  kind: M3ComponentKind;
  displayName: string;
  accelerator: AcceleratorKind | null;
  channel: M3ComponentChannel;
  activeVersionKey: string;
  versions: M3InstalledComponentVersion[];
}

export interface M3ComponentUpdateCheck {
  componentId: string;
  channel: M3ComponentChannel;
  installedVersion: string;
  installedPublishedAtMs: number;
  latestAvailable: M3ComponentCatalogEntry | null;
  updateAvailable: boolean;
}

export type SchedulerRuntimeKind = "ollama" | "llama_cpp";
export interface M3SchedulingInput {
  platform: PlatformCapabilities;
  memory: {
    available_ram_bytes: number;
    reserve_ram_bytes: number;
    available_vram_bytes: number;
    reserve_vram_bytes: number;
  };
  process_slots: Array<{
    slot_id: string;
    runtime: SchedulerRuntimeKind;
    port: number | null;
    state: { state: "available" } | { state: "occupied"; model_id: string; ownership: RunningModel["ownership"] };
  }>;
  residents: Array<{
    runtime: SchedulerRuntimeKind;
    model_id: string;
    memory: { ram_bytes: number; vram_bytes: number };
    ownership: RunningModel["ownership"];
    slot_id: string | null;
    port: number | null;
  }>;
  ports: Array<{
    port: number;
    owner_id: string;
    runtime: SchedulerRuntimeKind | null;
    ownership: RunningModel["ownership"];
  }>;
  targets: Array<{
    target_id: string;
    runtime: SchedulerRuntimeKind;
    model_id: string;
    memory: { ram_bytes: number; vram_bytes: number };
    accelerator: AcceleratorKind | null;
    preferred_slot_id: string | null;
  }>;
}

export interface M3SchedulingPlan {
  schema_version: number;
  waves: Array<{
    wave_index: number;
    ram_bytes: number;
    vram_bytes: number;
    targets: Array<{
      target_id: string;
      runtime: SchedulerRuntimeKind;
      model_id: string;
      process_slot_id: string | null;
      port: number | null;
      residency: "reuse_existing" | "load_transient";
      cleanup: "preserve" | "unload_app_managed";
      queued: boolean;
    }>;
  }>;
  preserved_residency: M3SchedulingInput["residents"];
}

export interface OffloadModelProfile {
  weights_bytes: number;
  estimated_ram_bytes: number;
  estimated_vram_bytes: number;
  required_accelerator: AcceleratorKind | null;
  has_vision_projector: boolean;
}

export interface OffloadPlanInput {
  hardware: HardwareSnapshot;
  model: OffloadModelProfile;
  reserved: { ram_bytes: number; vram_bytes: number };
  other_resident_count: number;
  requested_context_tokens: number | null;
}

export type ProjectorPlacement = "gpu" | "cpu" | "not_applicable";

export interface OffloadRationale {
  field: string;
  explanation: string;
}

export interface OffloadPlan {
  schema_version: number;
  accelerator: AcceleratorKind;
  context_tokens: number;
  requested_context_tokens: number;
  batch_size: number;
  gpu_layers: number;
  estimated_total_layers: number;
  cpu_spill_layers: number;
  projector_placement: ProjectorPlacement;
  parallel_sequences: number;
  available_ram_bytes: number;
  available_vram_bytes: number;
  rationale: OffloadRationale[];
  improvement_suggestions: string[];
}

export type SettingValue =
  | { type: "boolean"; value: boolean }
  | { type: "integer"; value: number }
  | { type: "float"; value: number }
  | { type: "text"; value: string }
  | { type: "choice"; value: string }
  | { type: "duration_ms"; value: number };

export type SettingValueSchema =
  | { type: "boolean" }
  | { type: "integer"; min: number; max: number; step: number }
  | { type: "float"; min: number; max: number; step: number }
  | { type: "text"; max_bytes: number }
  | { type: "choice"; options: string[] }
  | { type: "duration_ms"; min: number; max: number; step: number };

export interface AdvancedSettingCapability {
  key: string;
  label: string;
  description: string;
  schema: SettingValueSchema;
  default_value: SettingValue;
  restart_required: boolean;
}

export interface M3RuntimeDescriptor {
  runtimeId: string;
  kind: M3RuntimeKind;
  label: string;
  managed: boolean;
  apiBackend: ApiBackend;
}

export interface M3RuntimeCapability {
  descriptor: M3RuntimeDescriptor;
  canLoad: boolean;
  canUnload: boolean;
  canLogs: boolean;
  canMetrics: boolean;
  canInfer: boolean;
  settings: AdvancedSettingCapability[];
}

export interface RuntimeDescriptor {
  schema_version: number;
  runtime_id: string;
  kind: "ollama" | "llama_cpp";
  label: string;
  endpoint: unknown;
  managed: boolean;
}

export interface RuntimeStatus {
  runtime: RuntimeDescriptor;
  state: "stopped" | "starting" | "ready" | "degraded" | "unreachable" | "error";
  version: string | null;
  process: Record<string, unknown> | null;
  message: string | null;
  checked_at_ms: number;
}

export interface RunningModel {
  runtime_id: string;
  model_id: string;
  size_bytes: number;
  memory_bytes: number;
  vram_bytes: number;
  digest: string | null;
  expires_at: string | null;
  ownership: "pre_existing" | "app_managed" | "external";
}

export type MlxRuntimeStatus =
  | { state: "unavailable"; capabilities: Record<string, unknown> }
  | { state: "not_installed"; capabilities: Record<string, unknown> }
  | { state: "stopped"; capabilities: Record<string, unknown>; package_version: string }
  | {
      state: "running";
      capabilities: Record<string, unknown>;
      package_version: string;
      handle: Record<string, unknown>;
      metrics: MlxProcessMetrics;
    };

export interface MlxProcessMetrics {
  processAlive: boolean;
  residentMemoryBytes: number;
  unifiedMemoryBytes: number;
  activeRequests: number;
  generatedTokens: number;
  tokensPerSecond: number | null;
  sampledAtMs: number;
}

export type M3RuntimeStatusView =
  | { runtimeType: "adapter"; status: RuntimeStatus; running_models: RunningModel[] }
  | { runtimeType: "mlx"; status: MlxRuntimeStatus };

export type M3RuntimeMetricsView =
  | { runtimeType: "adapter"; status: RuntimeStatus; running_models: RunningModel[] }
  | { runtimeType: "mlx"; metrics: MlxProcessMetrics | null; status: MlxRuntimeStatus };

export interface RuntimeModel {
  model_id: string;
  display_name: string;
  size_bytes: number;
  local_path: string | null;
  digest: string | null;
  modified_at: string | null;
  capabilities: { chat: boolean; embeddings: boolean; tool_calling: boolean; vision: boolean };
  metadata: Record<string, string>;
}

export interface RuntimeInventory {
  schema_version: number;
  runtime_id: string;
  models: RuntimeModel[];
  captured_at_ms: number;
}

export interface RuntimeLogTail {
  text: string;
  truncated: boolean;
}

export type KeepAlive = { mode: "duration_ms"; milliseconds: number } | { mode: "forever" };

export interface M3LoadModelRequest {
  runtimeId: string;
  assetId: string;
  keepAlive: KeepAlive | null;
  replaceExisting: boolean;
}

export interface M3UnloadModelRequest {
  runtimeId: string;
  modelId: string;
  forceExactOwner: boolean;
}

export type M3ApiCaller =
  | { type: "internal" }
  | { type: "external"; bearer_token: string; remote_address: string };

export interface M3ApiDispatchRequest {
  protocol: CompatibilityProtocol;
  runtimeId: string;
  requestId: string;
  body: number[];
  caller: M3ApiCaller;
  nowMs: number;
}

export interface M3ApiDispatchResponse {
  status: number;
  body: unknown;
}

export interface M3CancelInferenceRequest {
  protocol: CompatibilityProtocol;
  runtimeId: string;
  requestId: string;
  modelId: string;
  caller: M3ApiCaller;
  nowMs: number;
}

export type TlsPolicy =
  | { mode: "disabled" }
  | {
      mode: "certificate";
      certificate_sha256: string;
      private_key_reference: string;
      minimum_version: "1.2" | "1.3";
    };

export interface LanServerPolicy {
  bindAddress: string;
  port: number;
  requireAuthentication: boolean;
  pairingRequired: boolean;
  tls: TlsPolicy;
  corsAllowlist: string[];
  allowedBackends: ApiBackend[];
  allowedLanMutations: ApiScope[];
  allowCloudProvidersOverLan: boolean;
  rateLimit: { windowMs: number; maxRequests: number; maxInputBytes: number };
  pairingTtlMs: number;
}

export interface PairingRequest {
  clientLabel: string;
  scopes: ApiScope[];
  backends: ApiBackend[];
  allowedModels: string[];
  tokenExpiresAtMs: number | null;
}

export interface PairingChallenge {
  challengeId: string;
  pairingCode: string;
  expiresAtMs: number;
  clientLabel: string;
}

export interface ScopedToken {
  tokenId: string;
  clientLabel: string;
  scopes: ApiScope[];
  backends: ApiBackend[];
  allowedModels: string[];
  createdAtMs: number;
  expiresAtMs: number | null;
  revokedAtMs: number | null;
  lastUsedAtMs: number | null;
}

export interface PairedToken {
  token: string;
  record: ScopedToken;
}

export interface SecurityAuditEvent {
  eventId: string;
  occurredAtMs: number;
  kind:
    | "pairing_started"
    | "pairing_failed"
    | "pairing_completed"
    | "token_authorized"
    | "token_denied"
    | "token_rate_limited"
    | "token_revoked";
  tokenId: string | null;
  challengeId: string | null;
  scope: ApiScope | null;
  remoteAddress: string | null;
  outcome: string;
  detail: string;
}

export interface M3HttpServerStatus {
  status: "stopped" | "starting" | "running" | "error";
  bindAddress: string | null;
  port: number | null;
  tls: boolean;
  startedAtMs: number | null;
  requestCount: number;
  activeRequests: number;
  lastRequestAtMs: number | null;
  lastError: string | null;
}

export interface OperationArgs extends Record<string, unknown> {
  operationId: string;
  timeoutMs?: number | null;
}

export function createM3OperationId(prefix: string): string {
  const suffix = globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return `${prefix}-${suffix}`;
}

export async function sha256Text(value: string): Promise<string> {
  const digest = await globalThis.crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export const runtimeHubClient = {
  hardwareSnapshot: () => invoke<HardwareSnapshot>("m3_hardware_snapshot"),
  hardwareProfile: () => invoke<HardwareProfile>("m3_hardware_profile"),
  hardwareCompatibilityReport: () =>
    invoke<M3HardwareCompatibilityReport>("m3_hardware_compatibility_report"),
  storageStatus: () => invoke<M3StorageStatus>("m3_storage_status"),
  installedModels: () => invoke<M3InstalledModel[]>("m3_installed_models"),
  catalogSources: () => invoke<M3CatalogSourceConfig[]>("m3_catalog_sources"),
  catalogReplaceSources: (sources: M3CatalogSourceConfig[]) =>
    invoke<M3CatalogSourceConfig[]>("m3_catalog_replace_sources", { sources }),
  runtimes: () => invoke<M3RuntimeCapability[]>("m3_runtimes"),
  refreshRuntimes: (args: OperationArgs) =>
    invoke<M3RuntimeCapability[]>("m3_refresh_runtimes", args),
  schedulePlan: (input: M3SchedulingInput) =>
    invoke<M3SchedulingPlan>("m3_schedule_plan", { input }),
  chatTemplateLabReport: (template: string | null) =>
    invoke<ChatTemplateLabReport>("m3_chat_template_lab_report", { template }),
  offloadPlan: (input: OffloadPlanInput) => invoke<OffloadPlan>("m3_offload_plan", { input }),
  catalogSearch: (args: OperationArgs & { query: string; limit: number }) =>
    invoke<M3CatalogMatch[]>("m3_catalog_search", args),
  modelDownload: (args: OperationArgs & { request: { model: M3CatalogModel; acceptedLicenseSha256: string } }) =>
    invoke<M3InstalledModel>("m3_model_download", args),
  modelUpdate: (
    args: OperationArgs & { assetId: string; request: { model: M3CatalogModel; acceptedLicenseSha256: string } },
  ) => invoke<M3InstalledModel>("m3_model_update", args),
  modelActivateVersion: (args: OperationArgs & { request: { assetId: string; versionKey: string } }) =>
    invoke<M3InstalledModel>("m3_model_activate_version", args),
  modelPruneVersions: (args: OperationArgs & { request: { assetId: string; confirmation: string } }) =>
    invoke<M3InstalledModel>("m3_model_prune_versions", args),
  modelDelete: (args: OperationArgs & { request: { assetId: string; confirmation: string } }) =>
    invoke<boolean>("m3_model_delete", args),
  cleanupOrphans: (args: OperationArgs & { confirmation: string }) =>
    invoke<M3CleanupReport>("m3_cleanup_orphans", args),
  cancelOperation: (operationId: string) => invoke<boolean>("m3_cancel_operation", { operationId }),
  runtimeStatus: (args: OperationArgs & { runtimeId: string }) =>
    invoke<M3RuntimeStatusView>("m3_runtime_status", args),
  runtimeInventory: (args: OperationArgs & { runtimeId: string }) =>
    invoke<RuntimeInventory>("m3_runtime_inventory", args),
  runtimeLoadModel: (args: OperationArgs & { request: M3LoadModelRequest }) =>
    invoke<void>("m3_runtime_load_model", args),
  runtimeUnloadModel: (args: OperationArgs & { request: M3UnloadModelRequest }) =>
    invoke<void>("m3_runtime_unload_model", args),
  runtimeLogs: (args: OperationArgs & { runtimeId: string; maxBytes: number }) =>
    invoke<RuntimeLogTail>("m3_runtime_logs", args),
  runtimeMetrics: (args: OperationArgs & { runtimeId: string }) =>
    invoke<M3RuntimeMetricsView>("m3_runtime_metrics", args),
  runtimeSetConfig: (request: { runtimeId: string; values: Record<string, SettingValue> }) =>
    invoke<Record<string, SettingValue>>("m3_runtime_set_config", { request }),
  runtimeConfig: (runtimeId: string) =>
    invoke<Record<string, SettingValue> | null>("m3_runtime_config", { runtimeId }),
  apiDispatch: (args: OperationArgs & { request: M3ApiDispatchRequest }) =>
    invoke<M3ApiDispatchResponse>("m3_api_dispatch", args),
  apiCancelInference: (args: OperationArgs & { request: M3CancelInferenceRequest }) =>
    invoke<boolean>("m3_api_cancel_inference", args),
  lanValidatePolicy: (policy: LanServerPolicy) => invoke<void>("m3_lan_validate_policy", { policy }),
  lanConfigure: (policy: LanServerPolicy) => invoke<LanServerPolicy>("m3_lan_configure", { policy }),
  lanDisable: (confirmation: string) => invoke<boolean>("m3_lan_disable", { confirmation }),
  lanPolicy: () => invoke<LanServerPolicy | null>("m3_lan_policy"),
  lanBeginPairing: (request: PairingRequest, nowMs: number, remoteAddress: string) =>
    invoke<PairingChallenge>("m3_lan_begin_pairing", { request, nowMs, remoteAddress }),
  lanCompletePairing: (challengeId: string, pairingCode: string, nowMs: number, remoteAddress: string) =>
    invoke<PairedToken>("m3_lan_complete_pairing", { challengeId, pairingCode, nowMs, remoteAddress }),
  lanRevokeToken: (tokenId: string, nowMs: number, remoteAddress: string) =>
    invoke<ScopedToken>("m3_lan_revoke_token", { tokenId, nowMs, remoteAddress }),
  lanTokens: () => invoke<ScopedToken[]>("m3_lan_tokens"),
  lanAuditEvents: () => invoke<SecurityAuditEvent[]>("m3_lan_audit_events"),
  httpServerStart: () => invoke<M3HttpServerStatus>("m3_http_server_start"),
  httpServerStop: () => invoke<M3HttpServerStatus>("m3_http_server_stop"),
  httpServerStatus: () => invoke<M3HttpServerStatus>("m3_http_server_status"),
  httpServerStoreTlsIdentity: (reference: string, certificatePem: string, privateKeyPem: string) =>
    invoke<string>("m3_http_server_store_tls_identity", { reference, certificatePem, privateKeyPem }),
  componentStorageStatus: () => invoke<M3StorageStatus>("m3_component_storage_status"),
  componentInstalled: () => invoke<M3InstalledComponent[]>("m3_component_installed"),
  componentRegistryEntries: () => invoke<M3ComponentCatalogEntry[]>("m3_component_registry_entries"),
  componentReplaceRegistryEntries: (entries: M3ComponentCatalogEntry[]) =>
    invoke<M3ComponentCatalogEntry[]>("m3_component_replace_registry_entries", { entries }),
  componentListRegistry: (args: OperationArgs) =>
    invoke<M3ComponentCatalogEntry[]>("m3_component_list_registry", args),
  componentCheckUpdates: (args: OperationArgs) =>
    invoke<M3ComponentUpdateCheck[]>("m3_component_check_updates", args),
  componentInstall: (args: OperationArgs & { request: { entry: M3ComponentCatalogEntry } }) =>
    invoke<M3InstalledComponent>("m3_component_install", args),
  componentActivateVersion: (
    args: OperationArgs & { request: { componentId: string; versionKey: string } },
  ) => invoke<M3InstalledComponent>("m3_component_activate_version", args),
};
