import { create } from "zustand";

/** Running token totals for one model label (see `describeUsageTarget` in
 * `turnEngine.ts`). */
export interface ModelUsageTotals {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  /** Number of completed turns (with a `usage` event) attributed to this model. */
  turns: number;
}

export interface UsageHistoryState {
  totalPromptTokens: number;
  totalCompletionTokens: number;
  totalTokens: number;
  /** Highest `totalTokens` seen in a single completed turn, ever. */
  peakTurnTokens: number;
  /** Local calendar date ("YYYY-MM-DD") -> total tokens used that day. */
  dailyTotals: Record<string, number>;
  /** Model label -> running totals for that model. */
  byModel: Record<string, ModelUsageTotals>;
  /** Number of completed `runAgentTurn` calls (fresh sends, edits, retries) — see `agentLoop.ts`. */
  totalTurns: number;
  /** Longest single `runAgentTurn` wall-clock duration ever seen, in milliseconds. */
  longestTurnMs: number;
  /** Number of tool invocations dispatched through `turnEngine.ts`'s `executeToolCall` (main turns and subagent turns alike). */
  toolCallsMade: number;
  /** Number of `runSubagentTask` invocations (the `task` tool) started. */
  subagentTasksRun: number;
  /** Number of `verify_run` Tauri command invocations kicked off (one per enabled verification command per turn). */
  verifyRuns: number;
  recordUsage: (modelLabel: string, usage: { promptTokens: number; completionTokens: number; totalTokens: number }) => void;
  recordTurnCompleted: (durationMs: number) => void;
  recordToolCall: () => void;
  recordSubagentTaskStarted: () => void;
  recordVerifyRun: () => void;
  clear: () => void;
}

/** localStorage key the usage history is persisted under after every recorded turn. */
export const STORAGE_KEY = "little-monkey-usage-history";

interface PersistedShape {
  totalPromptTokens: number;
  totalCompletionTokens: number;
  totalTokens: number;
  peakTurnTokens: number;
  dailyTotals: Record<string, number>;
  byModel: Record<string, ModelUsageTotals>;
  totalTurns: number;
  longestTurnMs: number;
  toolCallsMade: number;
  subagentTasksRun: number;
  verifyRuns: number;
}

function defaults(): PersistedShape {
  return {
    totalPromptTokens: 0,
    totalCompletionTokens: 0,
    totalTokens: 0,
    peakTurnTokens: 0,
    dailyTotals: {},
    byModel: {},
    totalTurns: 0,
    longestTurnMs: 0,
    toolCallsMade: 0,
    subagentTasksRun: 0,
    verifyRuns: 0,
  };
}

/** Defensive per-entry validation — one malformed/hand-edited entry must not corrupt the rest or crash hydration. */
function sanitizeDailyTotals(raw: unknown): Record<string, number> {
  if (!raw || typeof raw !== "object") return {};
  const out: Record<string, number> = {};
  for (const [date, value] of Object.entries(raw as Record<string, unknown>)) {
    if (/^\d{4}-\d{2}-\d{2}$/.test(date) && typeof value === "number" && Number.isFinite(value) && value >= 0) {
      out[date] = value;
    }
  }
  return out;
}

function sanitizeByModel(raw: unknown): Record<string, ModelUsageTotals> {
  if (!raw || typeof raw !== "object") return {};
  const out: Record<string, ModelUsageTotals> = {};
  for (const [model, value] of Object.entries(raw as Record<string, unknown>)) {
    if (!value || typeof value !== "object") continue;
    const entry = value as Partial<ModelUsageTotals>;
    if (
      typeof entry.promptTokens === "number" &&
      typeof entry.completionTokens === "number" &&
      typeof entry.totalTokens === "number" &&
      typeof entry.turns === "number"
    ) {
      out[model] = {
        promptTokens: entry.promptTokens,
        completionTokens: entry.completionTokens,
        totalTokens: entry.totalTokens,
        turns: entry.turns,
      };
    }
  }
  return out;
}

/** Loads the persisted usage blob, falling back to defaults for anything absent, corrupt, or malformed. */
function hydrate(): PersistedShape {
  const fallback = defaults();
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return fallback;
    return {
      totalPromptTokens: typeof parsed.totalPromptTokens === "number" ? parsed.totalPromptTokens : fallback.totalPromptTokens,
      totalCompletionTokens:
        typeof parsed.totalCompletionTokens === "number" ? parsed.totalCompletionTokens : fallback.totalCompletionTokens,
      totalTokens: typeof parsed.totalTokens === "number" ? parsed.totalTokens : fallback.totalTokens,
      peakTurnTokens: typeof parsed.peakTurnTokens === "number" ? parsed.peakTurnTokens : fallback.peakTurnTokens,
      dailyTotals: sanitizeDailyTotals(parsed.dailyTotals),
      byModel: sanitizeByModel(parsed.byModel),
      totalTurns: typeof parsed.totalTurns === "number" ? parsed.totalTurns : fallback.totalTurns,
      longestTurnMs: typeof parsed.longestTurnMs === "number" ? parsed.longestTurnMs : fallback.longestTurnMs,
      toolCallsMade: typeof parsed.toolCallsMade === "number" ? parsed.toolCallsMade : fallback.toolCallsMade,
      subagentTasksRun: typeof parsed.subagentTasksRun === "number" ? parsed.subagentTasksRun : fallback.subagentTasksRun,
      verifyRuns: typeof parsed.verifyRuns === "number" ? parsed.verifyRuns : fallback.verifyRuns,
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

/** Local calendar date key ("YYYY-MM-DD"), not UTC — a turn just after
 * midnight local time should land on today's bucket, matching the user's
 * own clock rather than UTC's. */
function todayKey(): string {
  const now = new Date();
  const year = now.getFullYear();
  const month = String(now.getMonth() + 1).padStart(2, "0");
  const day = String(now.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

const initial = hydrate();

/**
 * Persisted, cumulative usage history — lifetime/per-day/per-model token
 * totals plus a handful of activity counters — for the Settings "Usage" tab.
 * Unlike `usageStore.ts` (which holds only the most recent turn per session,
 * in memory, for the live context-usage ring), this store never overwrites:
 * every real `usage` event from `turnEngine.ts`'s `attemptStream` (main
 * turns, context-trim summarization, risk-judge calls) and from subagent
 * attempts in `subagent.ts` adds onto the running totals here, and the
 * activity counters (`totalTurns`, `toolCallsMade`, `subagentTasksRun`,
 * `verifyRuns`) are likewise real counts of things that actually happened —
 * never estimated or fabricated.
 */
export const useUsageHistoryStore = create<UsageHistoryState>((set, get) => ({
  ...initial,

  recordUsage: (modelLabel, usage) => {
    set((state) => {
      const date = todayKey();
      const existingModel = state.byModel[modelLabel] ?? { promptTokens: 0, completionTokens: 0, totalTokens: 0, turns: 0 };
      return {
        totalPromptTokens: state.totalPromptTokens + usage.promptTokens,
        totalCompletionTokens: state.totalCompletionTokens + usage.completionTokens,
        totalTokens: state.totalTokens + usage.totalTokens,
        peakTurnTokens: Math.max(state.peakTurnTokens, usage.totalTokens),
        dailyTotals: { ...state.dailyTotals, [date]: (state.dailyTotals[date] ?? 0) + usage.totalTokens },
        byModel: {
          ...state.byModel,
          [modelLabel]: {
            promptTokens: existingModel.promptTokens + usage.promptTokens,
            completionTokens: existingModel.completionTokens + usage.completionTokens,
            totalTokens: existingModel.totalTokens + usage.totalTokens,
            turns: existingModel.turns + 1,
          },
        },
      };
    });
    persist({ ...get() });
  },

  recordTurnCompleted: (durationMs) => {
    set((state) => ({
      totalTurns: state.totalTurns + 1,
      longestTurnMs: Math.max(state.longestTurnMs, durationMs),
    }));
    persist({ ...get() });
  },

  recordToolCall: () => {
    set((state) => ({ toolCallsMade: state.toolCallsMade + 1 }));
    persist({ ...get() });
  },

  recordSubagentTaskStarted: () => {
    set((state) => ({ subagentTasksRun: state.subagentTasksRun + 1 }));
    persist({ ...get() });
  },

  recordVerifyRun: () => {
    set((state) => ({ verifyRuns: state.verifyRuns + 1 }));
    persist({ ...get() });
  },

  clear: () => {
    const empty = defaults();
    set(empty);
    persist(empty);
  },
}));

export default useUsageHistoryStore;
