import type {
  ActiveProvider,
  EffortLevel,
  LlamaStatus,
  ModelInfo,
  OllamaModelInfo,
  ProviderConfig,
  ProviderModelInfo,
} from "../store/modelStore";

export type CapabilityState = "yes" | "no" | "unknown";

export interface CapabilityAssessment {
  readonly state: CapabilityState;
  readonly evidence: string;
}

export interface ModelCapabilities {
  readonly toolCalling: CapabilityAssessment;
  readonly vision: CapabilityAssessment;
}

export interface ModelTargetAvailability {
  readonly status: "available" | "unavailable";
  readonly evidence: string;
}

interface ModelTargetSnapshotBase {
  /** Canonical, persistence-safe identity for this exact backend/model pair. */
  readonly key: string;
  /** Backend/group label (for example, "Local", "Ollama", or "Anthropic"). */
  readonly label: string;
  /** Human-facing model name inside the backend group. */
  readonly displayName: string;
  readonly capabilities: ModelCapabilities;
  readonly availability: ModelTargetAvailability;
  /** Conservative resident-memory estimate for execution planning. Remote
   * targets use zero; older persisted snapshots may omit this field and are
   * treated as unknown by the planner. */
  readonly estimatedMemoryBytes?: number;
  /** Generation effort captured when the target is selected, when supported. */
  readonly effort?: EffortLevel;
}

export interface LocalModelTargetSnapshot extends ModelTargetSnapshotBase {
  readonly kind: "local";
  readonly modelId: string;
  readonly modelPath: string;
}

export interface OllamaModelTargetSnapshot extends ModelTargetSnapshotBase {
  readonly kind: "ollama";
  readonly baseUrl: string;
  readonly model: string;
  /** Cloud tags execute remotely and must not consume the local-memory
   * budget used to decide whether comparison branches can run together. */
  readonly isCloud?: boolean;
}

export interface ProviderModelTargetSnapshot extends ModelTargetSnapshotBase {
  readonly kind: "provider";
  readonly providerId: string;
  /** Frozen endpoint selected with this target. The Rust host revalidates it
   * against the provider configuration before accepting a durable run. */
  readonly endpoint: string;
  readonly model: string;
  /** Opaque keychain reference only; never the provider credential itself. */
  readonly credentialRefId: string;
}

export type ModelTargetSnapshot =
  | LocalModelTargetSnapshot
  | OllamaModelTargetSnapshot
  | ProviderModelTargetSnapshot;

export type ModelTargetKind = ModelTargetSnapshot["kind"];

export interface ModelTargetGroup {
  readonly key: string;
  readonly kind: ModelTargetKind;
  readonly label: string;
  readonly targets: readonly ModelTargetSnapshot[];
}

export interface ModelTargetInventory {
  readonly groups: readonly ModelTargetGroup[];
  readonly targets: readonly ModelTargetSnapshot[];
}

/** The read-only ModelStore subset needed to build target snapshots. */
export interface ModelTargetInventoryInput {
  readonly installed: readonly ModelInfo[];
  readonly active: ModelInfo | null;
  readonly llamaStatus: LlamaStatus;
  readonly ollamaModels: readonly OllamaModelInfo[];
  readonly ollamaReachable: boolean;
  readonly providers: readonly ProviderConfig[];
  readonly providerModels: Readonly<Record<string, readonly ProviderModelInfo[]>>;
  /** Per-model effort choices keyed by target key — see `modelStore.ts`'s
   * `effortByTarget`. A model with no entry snapshots no effort at all. */
  readonly effortByTarget?: Readonly<Record<string, EffortLevel>>;
  readonly ollamaBaseUrl?: string;
}

/** The read-only ModelStore subset needed to find its currently selected target. */
export interface ActiveModelTargetSelection {
  readonly activeProvider: ActiveProvider;
  readonly active: ModelInfo | null;
  readonly activeOllamaModel: string | null;
  readonly activeProviderId: string | null;
  readonly activeProviderModel: string | null;
}

export type ComparisonTargetValidationErrorCode =
  | "too_few_targets"
  | "too_many_targets"
  | "duplicate_target"
  | "multiple_local_targets";

export interface ComparisonTargetValidationError {
  readonly code: ComparisonTargetValidationErrorCode;
  readonly message: string;
}

export interface ComparisonTargetValidationResult {
  readonly valid: boolean;
  readonly errors: readonly ComparisonTargetValidationError[];
}

export const MIN_COMPARISON_TARGETS = 2;
export const MAX_COMPARISON_TARGETS = 4;
export const DEFAULT_OLLAMA_BASE_URL = "http://127.0.0.1:11434";

const EFFORT_LEVELS: readonly EffortLevel[] = ["low", "medium", "high", "xhigh", "max"];
const MODEL_RUNTIME_OVERHEAD_MULTIPLIER = 1.2;
const MODEL_RUNTIME_FIXED_OVERHEAD_BYTES = 512 * 1024 * 1024;

function estimatedResidentBytes(weightBytes: number): number {
  if (!Number.isFinite(weightBytes) || weightBytes <= 0) return 0;
  return Math.ceil(weightBytes * MODEL_RUNTIME_OVERHEAD_MULTIPLIER + MODEL_RUNTIME_FIXED_OVERHEAD_BYTES);
}

function encodeKeyPart(value: string): string {
  return encodeURIComponent(value);
}

export function localModelTargetKey(modelId: string): string {
  return `local:${encodeKeyPart(modelId)}`;
}

export function ollamaModelTargetKey(model: string): string {
  return `ollama:${encodeKeyPart(model)}`;
}

export function providerModelTargetKey(providerId: string, model: string): string {
  return `provider:${encodeKeyPart(providerId)}:${encodeKeyPart(model)}`;
}

/** Providers with a reasoning-effort knob, and the levels each accepts.
 * Anthropic's native `output_config.effort` takes all five levels; OpenAI,
 * Gemini, and OpenRouter expose a three-level `reasoning_effort` scale (the
 * Rust proxy clamps `xhigh`/`max` down to `high` on the wire — see
 * `providers.rs::build_chat_request`). Custom providers are deliberately
 * absent: their endpoints are unknowable and OpenAI-compatible servers
 * commonly hard-reject a `reasoning_effort` field on non-reasoning models,
 * so no effort is ever captured or sent for them. */
const PROVIDER_EFFORT_LEVELS: Readonly<Record<string, readonly EffortLevel[]>> = {
  anthropic: EFFORT_LEVELS,
  openai: ["low", "medium", "high"],
  gemini: ["low", "medium", "high"],
  openrouter: ["low", "medium", "high"],
};

/** The effort levels selectable for `providerId`, or `null` when the
 * provider has no effort knob at all (custom endpoints, unknown ids). */
export function effortLevelsForProvider(providerId: string): readonly EffortLevel[] | null {
  return PROVIDER_EFFORT_LEVELS[providerId] ?? null;
}

/** Provider-scope fallback entry `modelStore.ts`'s one-time migration seeds
 * from the legacy single-global (Anthropic-only) effort setting. It applies
 * to any Anthropic model without its own per-model entry, preserving the
 * pre-migration behavior where one slider covered every Anthropic model. */
export const ANTHROPIC_EFFORT_FALLBACK_KEY = "provider:anthropic";

/** Resolves the effort to use for one provider model from a per-target map:
 * the model's own entry first, then (Anthropic only) the migrated legacy
 * fallback. `undefined` means "send no effort field at all". */
export function effortForProviderModel(
  effortByTarget: Readonly<Record<string, EffortLevel>> | undefined,
  providerId: string,
  model: string,
): EffortLevel | undefined {
  const exact = effortByTarget?.[providerModelTargetKey(providerId, model)];
  if (exact) return exact;
  return providerId === "anthropic" ? effortByTarget?.[ANTHROPIC_EFFORT_FALLBACK_KEY] : undefined;
}

function capability(state: CapabilityState, evidence: string): CapabilityAssessment {
  return Object.freeze({ state, evidence });
}

function capabilities(toolCalling: CapabilityAssessment, vision: CapabilityAssessment): ModelCapabilities {
  return Object.freeze({ toolCalling, vision });
}

function availability(status: ModelTargetAvailability["status"], evidence: string): ModelTargetAvailability {
  return Object.freeze({ status, evidence });
}

function freezeTarget<T extends ModelTargetSnapshot>(target: T): T {
  return Object.freeze(target);
}

function normalizedBaseUrl(baseUrl: string | undefined): string {
  const trimmed = baseUrl?.trim().replace(/\/+$/, "");
  return trimmed || DEFAULT_OLLAMA_BASE_URL;
}

function localTarget(model: ModelInfo): LocalModelTargetSnapshot {
  return freezeTarget({
    kind: "local",
    key: localModelTargetKey(model.id),
    label: "Local",
    displayName: model.name,
    modelId: model.id,
    modelPath: model.path as string,
    estimatedMemoryBytes: estimatedResidentBytes(model.size_gb * 1_000_000_000),
    capabilities: capabilities(
      capability(model.tool_calling ? "yes" : "no", `Local model metadata reports tool_calling=${model.tool_calling}.`),
      capability("unknown", "Local model metadata does not report vision capability."),
    ),
    availability: availability("available", "The local llama.cpp server reports ready for this model."),
  });
}

function ollamaTarget(
  model: OllamaModelInfo,
  baseUrl: string,
  reachable: boolean,
): OllamaModelTargetSnapshot {
  return freezeTarget({
    kind: "ollama",
    key: ollamaModelTargetKey(model.name),
    label: "Ollama",
    displayName: model.name,
    baseUrl,
    model: model.name,
    isCloud: model.is_cloud,
    estimatedMemoryBytes: model.is_cloud ? 0 : estimatedResidentBytes(model.size_bytes),
    capabilities: capabilities(
      capability(model.tool_calling ? "yes" : "no", `Ollama model metadata reports tool_calling=${model.tool_calling}.`),
      capability(model.vision ? "yes" : "no", `Ollama model metadata reports vision=${model.vision}.`),
    ),
    availability: availability(
      reachable ? "available" : "unavailable",
      reachable ? "The Ollama daemon is reachable." : "The Ollama daemon is not reachable.",
    ),
  });
}

function providerTarget(
  provider: ProviderConfig,
  model: ProviderModelInfo,
  effortByTarget: Readonly<Record<string, EffortLevel>> | undefined,
): ProviderModelTargetSnapshot {
  const effort = effortLevelsForProvider(provider.id)
    ? effortForProviderModel(effortByTarget, provider.id, model.id)
    : undefined;
  const snapshot: ProviderModelTargetSnapshot = {
    kind: "provider",
    key: providerModelTargetKey(provider.id, model.id),
    label: provider.label,
    displayName: model.id,
    providerId: provider.id,
    endpoint: normalizedBaseUrl(provider.base_url),
    model: model.id,
    credentialRefId: `keychain:com.littlemonkey.app:${provider.id}`,
    estimatedMemoryBytes: 0,
    capabilities: capabilities(
      capability("unknown", "The provider model inventory does not report tool-calling capability."),
      capability("unknown", "The provider model inventory does not report vision capability."),
    ),
    availability: availability(
      "available",
      "Provider credentials are configured; request reachability is checked when the turn starts.",
    ),
    ...(effort ? { effort } : {}),
  };
  return freezeTarget(snapshot);
}

function freezeGroup(group: ModelTargetGroup): ModelTargetGroup {
  return Object.freeze({ ...group, targets: Object.freeze([...group.targets]) });
}

/**
 * Converts model-store-shaped data into immutable, serializable targets.
 * Only the currently running, ready chat model is eligible for llama.cpp;
 * connected provider models and all known Ollama tags remain grouped by backend.
 */
export function buildModelTargetInventory(input: ModelTargetInventoryInput): ModelTargetInventory {
  const groups: ModelTargetGroup[] = [];
  const seenKeys = new Set<string>();

  const activeLocal = input.active;
  const eligibleLocal =
    input.llamaStatus === "ready" &&
    activeLocal?.kind === "chat" &&
    activeLocal.installed &&
    typeof activeLocal.path === "string" &&
    activeLocal.path.length > 0
      ? input.installed.find(
          (model) =>
            model.kind === "chat" &&
            model.installed &&
            model.path !== null &&
            (model.path === activeLocal.path || model.id === activeLocal.id),
        ) ?? null
      : null;

  if (eligibleLocal) {
    const target = localTarget(eligibleLocal);
    seenKeys.add(target.key);
    groups.push(freezeGroup({ key: "local", kind: "local", label: "Local", targets: [target] }));
  }

  for (const provider of input.providers) {
    if (!provider.has_key || !provider.id.trim()) continue;
    const targets: ProviderModelTargetSnapshot[] = [];
    for (const model of input.providerModels[provider.id] ?? []) {
      if (!model.id.trim()) continue;
      const target = providerTarget(provider, model, input.effortByTarget);
      if (seenKeys.has(target.key)) continue;
      seenKeys.add(target.key);
      targets.push(target);
    }
    if (targets.length > 0) {
      groups.push(
        freezeGroup({
          key: `provider:${encodeKeyPart(provider.id)}`,
          kind: "provider",
          label: provider.label,
          targets,
        }),
      );
    }
  }

  const ollamaTargets: OllamaModelTargetSnapshot[] = [];
  const baseUrl = normalizedBaseUrl(input.ollamaBaseUrl);
  for (const model of input.ollamaModels) {
    if (!model.name.trim()) continue;
    const target = ollamaTarget(model, baseUrl, input.ollamaReachable);
    if (seenKeys.has(target.key)) continue;
    seenKeys.add(target.key);
    ollamaTargets.push(target);
  }
  if (ollamaTargets.length > 0) {
    groups.push(freezeGroup({ key: "ollama", kind: "ollama", label: "Ollama", targets: ollamaTargets }));
  }

  const frozenGroups = Object.freeze(groups);
  return Object.freeze({
    groups: frozenGroups,
    targets: Object.freeze(frozenGroups.flatMap((group) => group.targets)),
  });
}

/** Returns the selected target only when it exists in this inventory snapshot. */
export function findActiveModelTarget(
  inventory: ModelTargetInventory | readonly ModelTargetSnapshot[],
  selection: ActiveModelTargetSelection,
): ModelTargetSnapshot | null {
  const targets: readonly ModelTargetSnapshot[] = "targets" in inventory ? inventory.targets : inventory;
  if (selection.activeProvider === "local") {
    if (!selection.active) return null;
    const key = localModelTargetKey(selection.active.id);
    return targets.find((target) => target.key === key && target.kind === "local") ?? null;
  }
  if (selection.activeProvider === "ollama") {
    if (!selection.activeOllamaModel) return null;
    const key = ollamaModelTargetKey(selection.activeOllamaModel);
    return targets.find((target) => target.key === key && target.kind === "ollama") ?? null;
  }
  if (!selection.activeProviderId || !selection.activeProviderModel) return null;
  const key = providerModelTargetKey(selection.activeProviderId, selection.activeProviderModel);
  return targets.find((target) => target.key === key && target.kind === "provider") ?? null;
}

export function validateComparisonTargets(
  targets: readonly ModelTargetSnapshot[],
): ComparisonTargetValidationResult {
  const errors: ComparisonTargetValidationError[] = [];
  if (targets.length < MIN_COMPARISON_TARGETS) {
    errors.push({
      code: "too_few_targets",
      message: `Select at least ${MIN_COMPARISON_TARGETS} models to compare.`,
    });
  }
  if (targets.length > MAX_COMPARISON_TARGETS) {
    errors.push({
      code: "too_many_targets",
      message: `Select no more than ${MAX_COMPARISON_TARGETS} models to compare.`,
    });
  }
  if (new Set(targets.map((target) => target.key)).size !== targets.length) {
    errors.push({ code: "duplicate_target", message: "Each comparison target must be unique." });
  }
  if (targets.filter((target) => target.kind === "local").length > 1) {
    errors.push({
      code: "multiple_local_targets",
      message: "Only one local llama.cpp model can participate in a comparison.",
    });
  }
  return Object.freeze({ valid: errors.length === 0, errors: Object.freeze(errors) });
}

export function assertValidComparisonTargets(targets: readonly ModelTargetSnapshot[]): void {
  const result = validateComparisonTargets(targets);
  if (!result.valid) {
    throw new Error(result.errors.map((error) => error.message).join(" "));
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isCapabilityAssessment(value: unknown): value is CapabilityAssessment {
  return (
    isRecord(value) &&
    (value.state === "yes" || value.state === "no" || value.state === "unknown") &&
    typeof value.evidence === "string"
  );
}

function isCapabilities(value: unknown): value is ModelCapabilities {
  return (
    isRecord(value) &&
    isCapabilityAssessment(value.toolCalling) &&
    isCapabilityAssessment(value.vision)
  );
}

function isAvailability(value: unknown): value is ModelTargetAvailability {
  return (
    isRecord(value) &&
    (value.status === "available" || value.status === "unavailable") &&
    typeof value.evidence === "string"
  );
}

function hasValidCommonTargetFields(value: Record<string, unknown>): boolean {
  return (
    isNonEmptyString(value.key) &&
    isNonEmptyString(value.label) &&
    isNonEmptyString(value.displayName) &&
    isCapabilities(value.capabilities) &&
    isAvailability(value.availability) &&
    (value.estimatedMemoryBytes === undefined ||
      (typeof value.estimatedMemoryBytes === "number" &&
        Number.isFinite(value.estimatedMemoryBytes) &&
        value.estimatedMemoryBytes >= 0)) &&
    (value.effort === undefined || EFFORT_LEVELS.includes(value.effort as EffortLevel))
  );
}

/** Defensive persistence guard; malformed or non-canonical snapshots are rejected. */
export function isModelTargetSnapshot(value: unknown): value is ModelTargetSnapshot {
  if (!isRecord(value) || !hasValidCommonTargetFields(value)) return false;
  if (value.kind === "local") {
    return (
      isNonEmptyString(value.modelId) &&
      isNonEmptyString(value.modelPath) &&
      value.key === localModelTargetKey(value.modelId)
    );
  }
  if (value.kind === "ollama") {
    return (
      isNonEmptyString(value.baseUrl) &&
      isNonEmptyString(value.model) &&
      (value.isCloud === undefined || typeof value.isCloud === "boolean") &&
      value.key === ollamaModelTargetKey(value.model)
    );
  }
  if (value.kind === "provider") {
    return (
      isNonEmptyString(value.providerId) &&
      isNonEmptyString(value.endpoint) &&
      isNonEmptyString(value.model) &&
      isNonEmptyString(value.credentialRefId) &&
      value.key === providerModelTargetKey(value.providerId, value.model)
    );
  }
  return false;
}
