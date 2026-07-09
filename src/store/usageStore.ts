import { create } from "zustand";

/**
 * Token usage reported by the model's own streaming HTTP response for the
 * most recently completed turn (see the `usage` `StreamEvent` variant in
 * `src/lib/llamaClient.ts`). This store only ever reflects real numbers
 * parsed off the wire — never fabricated subscription/quota data.
 */
export interface UsageInfo {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export interface UsageStore {
  /** Usage for the most recent completed turn, or `null` before any turn has reported usage. */
  lastUsage: UsageInfo | null;
  /**
   * The active model's context window size in tokens, or `null` when it
   * isn't known (e.g. a remote model/provider that doesn't expose one).
   */
  contextLimit: number | null;
  setUsage: (usage: UsageInfo) => void;
  setContextLimit: (limit: number | null) => void;
}

/**
 * Small, standalone zustand store holding the latest context-window token
 * usage. Intentionally has no `invoke` calls or event listeners of its own —
 * it's a pure state container that other code (agentLoop.ts, modelStore.ts)
 * pushes values into.
 */
export const useUsageStore = create<UsageStore>((set) => ({
  lastUsage: null,
  contextLimit: null,

  setUsage: (usage) => set({ lastUsage: usage }),
  setContextLimit: (limit) => set({ contextLimit: limit }),
}));

export default useUsageStore;
