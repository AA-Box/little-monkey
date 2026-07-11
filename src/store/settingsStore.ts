import { create } from "zustand";

/** localStorage key the full settings blob is persisted under after every mutation.
 * Exported so tests can clear it and re-import the module to genuinely
 * exercise `hydrate()`'s default-fallback path, rather than asserting
 * against a store state a test set up by hand. */
export const STORAGE_KEY = "little-monkey-automation-settings";

/** How aggressively to reclaim context-window space once `contextTrimThreshold` is crossed. */
export type ContextTrimStrategy = "trim" | "summarize";

/** A user-entered (never assumed) request-rate ceiling for one provider, used only to warn — see `rateLimitTracker.ts`. */
export interface ProviderRateLimit {
  /** Requests per minute the user wants to be warned when approaching. */
  rpm?: number;
  /** Requests per day the user wants to be warned when approaching. */
  rpd?: number;
}

export interface SettingsState {
  /** Retry the next configured cloud provider when one errors before any content streams back. */
  autoFailoverEnabled: boolean;
  /** Auto-switch to a vision-capable model when an image is attached and the active one can't see. */
  autoVisionSwitchEnabled: boolean;
  /** Automatically compact conversation history once it crosses `contextTrimThreshold`. */
  contextTrimEnabled: boolean;
  /** Percent of the active model's context window that triggers a compaction (0-100). */
  contextTrimThreshold: number;
  /** "trim" drops the oldest messages instantly; "summarize" spends one extra model call to compact them into a note. */
  contextTrimStrategy: ContextTrimStrategy;
  /** Show a warning when a provider's tracked request rate approaches a user-entered cap. */
  rateLimitWarningsEnabled: boolean;
  /** Provider id -> user-entered rate ceiling. Empty/absent means "no cap configured, never warn for this provider". */
  providerRateLimits: Record<string, ProviderRateLimit>;
  /** "providerId:modelId" -> manual correction of the built-in vision-capability heuristic (see `visionModels.ts`). */
  visionOverrides: Record<string, boolean>;
  /** Provider id -> user-curated model allowlist for that provider's model list (e.g. the OpenRouter tab's picker). Absent/`showAll: true` means unfiltered. */
  providerModelFilters: Record<string, ProviderModelFilter>;
  /** How many finished checkpoints (see checkpoints.rs) to keep on disk before the oldest are pruned — passed as `checkpoint_begin`'s `max_keep` param. Range 5-100, default 20 (mirrors the backend's own `MAX_CHECKPOINTS` fallback). */
  checkpointRetention: number;
  /** Whether the `remember` tool is offered to the model this turn (see `agentLoop.ts`'s `TOOLS` filter). Default true. Turning this off is not amnesia: rules and previously-saved facts are still injected into every system prompt regardless — it only stops the agent from saving *new* facts on its own. Facts remain manually addable/editable/deletable in the Rules tab either way. */
  memoryEnabled: boolean;
  /** Whether the `web_fetch`/`web_search` tools are offered to the model this turn (see `agentLoop.ts`'s `toolsForSettings` filter). Default true — DuckDuckGo search needs no key, so web research works out of the box, and the permission prompt shown for every call is the real gate. Turning this off makes both tools invisible to the model (not merely denied), mirroring `memoryEnabled`'s "disabled = not offered" treatment of `remember`. */
  webToolsEnabled: boolean;
  /** Whether `runAgentTurnBody` auto-runs the current workspace's enabled verification commands (see `src-tauri/src/verify.rs`) after a turn that wrote files. Default false — running arbitrary configured shell automatically should be opt-in, mirroring `memoryEnabled`'s posture. This slice is report-only: results are appended as `[Verify]` notices, nothing is fed back to the model yet (that's a later slice's `verifyMaxRounds`). */
  verifyEnabled: boolean;
  /** How many times `runAgentTurnBody` will feed a failed verification command's output back to the model as a fix instruction and let the loop continue, before leaving the failure notice as-is. Range 0-3 (mirrors Aider's `max_reflections=3`); default 1. 0 means report-only — the same behavior as before this setting existed. */
  verifyMaxRounds: number;
  /** Whether `write_file`/`edit_file`/`run_shell` calls get an LLM-judged risk classification (low/medium/high + a short reason) attached to their permission prompt — see `riskJudge.ts`'s `classifyToolCall` and `agentLoop.ts`'s `runAgentTurnBody`. Default false: it costs one extra model call per mutating tool call. Purely advisory in every mode as of Phase 2 (docs/roadmap/p2-plan-act-safety.md) — turning this on changes what the permission prompt *shows*, never what gets auto-approved. */
  riskAnnotationsEnabled: boolean;

  setAutoFailoverEnabled: (value: boolean) => void;
  setAutoVisionSwitchEnabled: (value: boolean) => void;
  setContextTrimEnabled: (value: boolean) => void;
  setContextTrimThreshold: (value: number) => void;
  setContextTrimStrategy: (value: ContextTrimStrategy) => void;
  setRateLimitWarningsEnabled: (value: boolean) => void;
  setProviderRateLimit: (providerId: string, limit: ProviderRateLimit) => void;
  clearProviderRateLimit: (providerId: string) => void;
  setVisionOverride: (key: string, value: boolean) => void;
  clearVisionOverride: (key: string) => void;
  setProviderModelShowAll: (providerId: string, showAll: boolean) => void;
  toggleProviderModelSelected: (providerId: string, modelId: string) => void;
  clearProviderModelSelection: (providerId: string) => void;
  setCheckpointRetention: (value: number) => void;
  setMemoryEnabled: (value: boolean) => void;
  setWebToolsEnabled: (value: boolean) => void;
  setVerifyEnabled: (value: boolean) => void;
  setVerifyMaxRounds: (value: number) => void;
  setRiskAnnotationsEnabled: (value: boolean) => void;
}

/** A provider's curated model list: which ids to show, and whether to bypass curation entirely. */
export interface ProviderModelFilter {
  /** When true, every model for this provider is shown regardless of `selectedModelIds` — lets a user keep favorites checked while still browsing the full list. */
  showAll: boolean;
  /** Model ids the user has explicitly checked. Ignored while `showAll` is true, and also ignored (i.e. treated as "show everything") when empty, so a freshly-connected provider isn't curated down to nothing before the user has picked anything. */
  selectedModelIds: string[];
}

/**
 * Stable "no curation yet" fallback — must be a module-level constant, not a
 * fresh object literal inlined in a selector, for the same reason
 * `ProviderCard.tsx`'s `EMPTY_MODELS` is: a new object every render makes
 * Zustand see a "changed" snapshot on every render and spin into an infinite
 * re-render loop.
 */
export const DEFAULT_PROVIDER_MODEL_FILTER: ProviderModelFilter = { showAll: true, selectedModelIds: [] };

const DEFAULT_CONTEXT_TRIM_THRESHOLD = 85;
/** Mirrors `MAX_CHECKPOINTS` in src-tauri/src/checkpoints.rs — the backend's
 * own fallback when no `max_keep` is supplied, kept in sync here so the
 * setting's default matches pre-existing behavior. */
const DEFAULT_CHECKPOINT_RETENTION = 20;
export const MIN_CHECKPOINT_RETENTION = 5;
export const MAX_CHECKPOINT_RETENTION = 100;
/** Mirrors Aider's `max_reflections=3` — see `verifyMaxRounds`'s doc comment. */
const DEFAULT_VERIFY_MAX_ROUNDS = 1;
export const MIN_VERIFY_MAX_ROUNDS = 0;
export const MAX_VERIFY_MAX_ROUNDS = 3;

interface PersistedShape {
  autoFailoverEnabled: boolean;
  autoVisionSwitchEnabled: boolean;
  contextTrimEnabled: boolean;
  contextTrimThreshold: number;
  contextTrimStrategy: ContextTrimStrategy;
  rateLimitWarningsEnabled: boolean;
  providerRateLimits: Record<string, ProviderRateLimit>;
  visionOverrides: Record<string, boolean>;
  providerModelFilters: Record<string, ProviderModelFilter>;
  checkpointRetention: number;
  memoryEnabled: boolean;
  webToolsEnabled: boolean;
  verifyEnabled: boolean;
  verifyMaxRounds: number;
  riskAnnotationsEnabled: boolean;
}

function defaults(): PersistedShape {
  return {
    autoFailoverEnabled: true,
    autoVisionSwitchEnabled: true,
    contextTrimEnabled: true,
    contextTrimThreshold: DEFAULT_CONTEXT_TRIM_THRESHOLD,
    contextTrimStrategy: "summarize",
    rateLimitWarningsEnabled: true,
    providerRateLimits: {},
    visionOverrides: {},
    providerModelFilters: {},
    checkpointRetention: DEFAULT_CHECKPOINT_RETENTION,
    memoryEnabled: true,
    webToolsEnabled: true,
    verifyEnabled: false,
    verifyMaxRounds: DEFAULT_VERIFY_MAX_ROUNDS,
    riskAnnotationsEnabled: false,
  };
}

/** Defensive per-entry validation for a persisted `providerModelFilters` blob — one malformed entry (e.g. hand-edited localStorage) must not corrupt the rest. */
function sanitizeProviderModelFilters(raw: unknown): Record<string, ProviderModelFilter> {
  if (!raw || typeof raw !== "object") return {};
  const out: Record<string, ProviderModelFilter> = {};
  for (const [providerId, value] of Object.entries(raw as Record<string, unknown>)) {
    if (!value || typeof value !== "object") continue;
    const entry = value as Partial<ProviderModelFilter>;
    out[providerId] = {
      showAll: typeof entry.showAll === "boolean" ? entry.showAll : true,
      selectedModelIds: Array.isArray(entry.selectedModelIds)
        ? entry.selectedModelIds.filter((id): id is string => typeof id === "string")
        : [],
    };
  }
  return out;
}

/** Loads the persisted settings blob, falling back to defaults for anything absent, corrupt, or malformed. */
function hydrate(): PersistedShape {
  const fallback = defaults();
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return fallback;
    return {
      autoFailoverEnabled: typeof parsed.autoFailoverEnabled === "boolean" ? parsed.autoFailoverEnabled : fallback.autoFailoverEnabled,
      autoVisionSwitchEnabled:
        typeof parsed.autoVisionSwitchEnabled === "boolean" ? parsed.autoVisionSwitchEnabled : fallback.autoVisionSwitchEnabled,
      contextTrimEnabled: typeof parsed.contextTrimEnabled === "boolean" ? parsed.contextTrimEnabled : fallback.contextTrimEnabled,
      contextTrimThreshold:
        typeof parsed.contextTrimThreshold === "number" && parsed.contextTrimThreshold > 0 && parsed.contextTrimThreshold <= 100
          ? parsed.contextTrimThreshold
          : fallback.contextTrimThreshold,
      contextTrimStrategy: parsed.contextTrimStrategy === "trim" || parsed.contextTrimStrategy === "summarize"
        ? parsed.contextTrimStrategy
        : fallback.contextTrimStrategy,
      rateLimitWarningsEnabled:
        typeof parsed.rateLimitWarningsEnabled === "boolean" ? parsed.rateLimitWarningsEnabled : fallback.rateLimitWarningsEnabled,
      providerRateLimits:
        parsed.providerRateLimits && typeof parsed.providerRateLimits === "object" ? parsed.providerRateLimits : fallback.providerRateLimits,
      visionOverrides: parsed.visionOverrides && typeof parsed.visionOverrides === "object" ? parsed.visionOverrides : fallback.visionOverrides,
      providerModelFilters: sanitizeProviderModelFilters(parsed.providerModelFilters),
      checkpointRetention:
        typeof parsed.checkpointRetention === "number" &&
        parsed.checkpointRetention >= MIN_CHECKPOINT_RETENTION &&
        parsed.checkpointRetention <= MAX_CHECKPOINT_RETENTION
          ? Math.round(parsed.checkpointRetention)
          : fallback.checkpointRetention,
      memoryEnabled: typeof parsed.memoryEnabled === "boolean" ? parsed.memoryEnabled : fallback.memoryEnabled,
      webToolsEnabled: typeof parsed.webToolsEnabled === "boolean" ? parsed.webToolsEnabled : fallback.webToolsEnabled,
      verifyEnabled: typeof parsed.verifyEnabled === "boolean" ? parsed.verifyEnabled : fallback.verifyEnabled,
      verifyMaxRounds:
        typeof parsed.verifyMaxRounds === "number" &&
        parsed.verifyMaxRounds >= MIN_VERIFY_MAX_ROUNDS &&
        parsed.verifyMaxRounds <= MAX_VERIFY_MAX_ROUNDS
          ? Math.round(parsed.verifyMaxRounds)
          : fallback.verifyMaxRounds,
      riskAnnotationsEnabled:
        typeof parsed.riskAnnotationsEnabled === "boolean" ? parsed.riskAnnotationsEnabled : fallback.riskAnnotationsEnabled,
    };
  } catch {
    return fallback;
  }
}

/** Best-effort persist — a quota error or serialization issue must never throw into the caller. */
function persist(state: PersistedShape): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Ignore — persistence is best-effort.
  }
}

const initial = hydrate();

export const useSettingsStore = create<SettingsState>((set, get) => ({
  ...initial,

  setAutoFailoverEnabled: (value) => {
    set({ autoFailoverEnabled: value });
    persist({ ...get() });
  },

  setAutoVisionSwitchEnabled: (value) => {
    set({ autoVisionSwitchEnabled: value });
    persist({ ...get() });
  },

  setContextTrimEnabled: (value) => {
    set({ contextTrimEnabled: value });
    persist({ ...get() });
  },

  setContextTrimThreshold: (value) => {
    const clamped = Math.min(100, Math.max(1, Math.round(value)));
    set({ contextTrimThreshold: clamped });
    persist({ ...get() });
  },

  setContextTrimStrategy: (value) => {
    set({ contextTrimStrategy: value });
    persist({ ...get() });
  },

  setRateLimitWarningsEnabled: (value) => {
    set({ rateLimitWarningsEnabled: value });
    persist({ ...get() });
  },

  setProviderRateLimit: (providerId, limit) => {
    set((state) => ({ providerRateLimits: { ...state.providerRateLimits, [providerId]: limit } }));
    persist({ ...get() });
  },

  clearProviderRateLimit: (providerId) => {
    set((state) => {
      const { [providerId]: _discard, ...rest } = state.providerRateLimits;
      return { providerRateLimits: rest };
    });
    persist({ ...get() });
  },

  setVisionOverride: (key, value) => {
    set((state) => ({ visionOverrides: { ...state.visionOverrides, [key]: value } }));
    persist({ ...get() });
  },

  clearVisionOverride: (key) => {
    set((state) => {
      const { [key]: _discard, ...rest } = state.visionOverrides;
      return { visionOverrides: rest };
    });
    persist({ ...get() });
  },

  setProviderModelShowAll: (providerId, showAll) => {
    set((state) => {
      const existing = state.providerModelFilters[providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
      return { providerModelFilters: { ...state.providerModelFilters, [providerId]: { ...existing, showAll } } };
    });
    persist({ ...get() });
  },

  toggleProviderModelSelected: (providerId, modelId) => {
    set((state) => {
      const existing = state.providerModelFilters[providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
      const selectedModelIds = existing.selectedModelIds.includes(modelId)
        ? existing.selectedModelIds.filter((id) => id !== modelId)
        : [...existing.selectedModelIds, modelId];
      return { providerModelFilters: { ...state.providerModelFilters, [providerId]: { ...existing, selectedModelIds } } };
    });
    persist({ ...get() });
  },

  clearProviderModelSelection: (providerId) => {
    set((state) => {
      const existing = state.providerModelFilters[providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
      return { providerModelFilters: { ...state.providerModelFilters, [providerId]: { ...existing, selectedModelIds: [] } } };
    });
    persist({ ...get() });
  },

  setCheckpointRetention: (value) => {
    const clamped = Math.min(MAX_CHECKPOINT_RETENTION, Math.max(MIN_CHECKPOINT_RETENTION, Math.round(value)));
    set({ checkpointRetention: clamped });
    persist({ ...get() });
  },

  setMemoryEnabled: (value) => {
    set({ memoryEnabled: value });
    persist({ ...get() });
  },

  setWebToolsEnabled: (value) => {
    set({ webToolsEnabled: value });
    persist({ ...get() });
  },

  setVerifyEnabled: (value) => {
    set({ verifyEnabled: value });
    persist({ ...get() });
  },

  setVerifyMaxRounds: (value) => {
    const clamped = Math.min(MAX_VERIFY_MAX_ROUNDS, Math.max(MIN_VERIFY_MAX_ROUNDS, Math.round(value)));
    set({ verifyMaxRounds: clamped });
    persist({ ...get() });
  },

  setRiskAnnotationsEnabled: (value) => {
    set({ riskAnnotationsEnabled: value });
    persist({ ...get() });
  },
}));

export default useSettingsStore;
