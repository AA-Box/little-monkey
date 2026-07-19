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
  /**
   * Usage for each session's most recent completed turn, keyed by session
   * id — with the split pane, two sessions can stream turns concurrently
   * and each pane's indicator must show its own session's numbers, not
   * whichever turn reported last.
   */
  usageBySession: Record<string, UsageInfo>;
  /**
   * The active model's context window size in tokens, or `null` when it
   * isn't known (e.g. a remote model/provider that doesn't expose one).
   * Model-global, unlike usage — both panes chat with the active model.
   */
  contextLimit: number | null;
  setUsage: (sessionId: string, usage: UsageInfo) => void;
  setContextLimit: (limit: number | null) => void;
}

/**
 * Small, standalone zustand store holding the latest context-window token
 * usage. Intentionally has no `invoke` calls or event listeners of its own —
 * it's a pure state container that other code (agentLoop.ts, modelStore.ts)
 * pushes values into.
 */
export const useUsageStore = create<UsageStore>((set) => ({
  usageBySession: {},
  contextLimit: null,

  setUsage: (sessionId, usage) =>
    set((state) => ({ usageBySession: { ...state.usageBySession, [sessionId]: usage } })),
  setContextLimit: (limit) => set({ contextLimit: limit }),
}));

export default useUsageStore;
