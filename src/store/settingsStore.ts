import { create } from "zustand";

/** localStorage key the full settings blob is persisted under after every mutation. */
const STORAGE_KEY = "little-monkey-automation-settings";

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
}

const DEFAULT_CONTEXT_TRIM_THRESHOLD = 85;

interface PersistedShape {
  autoFailoverEnabled: boolean;
  autoVisionSwitchEnabled: boolean;
  contextTrimEnabled: boolean;
  contextTrimThreshold: number;
  contextTrimStrategy: ContextTrimStrategy;
  rateLimitWarningsEnabled: boolean;
  providerRateLimits: Record<string, ProviderRateLimit>;
  visionOverrides: Record<string, boolean>;
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
  };
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
}));

export default useSettingsStore;
