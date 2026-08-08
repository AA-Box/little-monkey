import { create } from "zustand";

import type { LabCostRate } from "../lib/compareLab";

export const COST_CONTROL_STORAGE_KEY = "little-monkey-cost-controls-v1";
export const MAX_COST_USAGE_ENTRIES = 5_000;

export interface CostUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export interface CostUsageEntry {
  id: string;
  occurredAtMs: number;
  targetKey: string;
  targetLabel: string;
  sessionId: string;
  runId: string | null;
  usage: CostUsage;
  /** Null means the user has not configured a rate for this remote model. */
  costUsd: number | null;
  /** Measured milliseconds from request start to this attempt's first content
   * or tool-call fragment. Absent on entries written before this was recorded,
   * and null when the `usage` event arrived before any fragment did — both
   * mean "not measured", which is what `observedTimeToFirstTokenMs` (K9's
   * latency criterion) treats them as. This is the only latency number the
   * routing engine will act on; nothing here is vendor-published. */
  timeToFirstTokenMs?: number | null;
}

export type CostBudgetEnforcement = "warn" | "pause";

export interface CostBudgetPolicy {
  enabled: boolean;
  dailyBudgetUsd: number | null;
  monthlyBudgetUsd: number | null;
  warningPercent: number;
  enforcement: CostBudgetEnforcement;
}

export interface CostWindow {
  spentUsd: number;
  unknownCalls: number;
  knownCalls: number;
}

export interface CostBudgetEvaluation {
  status: "disabled" | "ok" | "warning" | "exceeded";
  daily: CostWindow;
  monthly: CostWindow;
  exceededWindows: Array<"daily" | "monthly">;
  warningWindows: Array<"daily" | "monthly">;
}

export interface CostControlState {
  policy: CostBudgetPolicy;
  rates: Record<string, LabCostRate>;
  entries: CostUsageEntry[];
  setPolicy: (patch: Partial<CostBudgetPolicy>) => void;
  setRate: (targetKey: string, rate: LabCostRate | null) => void;
  recordUsage: (entry: Omit<CostUsageEntry, "id">) => CostUsageEntry;
  clearUsage: () => void;
}

interface PersistedShape {
  version: 1;
  policy: CostBudgetPolicy;
  rates: Record<string, LabCostRate>;
  entries: CostUsageEntry[];
}

export const DEFAULT_COST_BUDGET_POLICY: CostBudgetPolicy = {
  enabled: false,
  dailyBudgetUsd: null,
  monthlyBudgetUsd: null,
  warningPercent: 0.8,
  enforcement: "warn",
};

function nonNegativeFinite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function optionalPositive(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : null;
}

function sanitizePolicy(value: unknown): CostBudgetPolicy {
  if (!value || typeof value !== "object") return { ...DEFAULT_COST_BUDGET_POLICY };
  const candidate = value as Partial<CostBudgetPolicy>;
  return {
    enabled: candidate.enabled === true,
    dailyBudgetUsd: optionalPositive(candidate.dailyBudgetUsd),
    monthlyBudgetUsd: optionalPositive(candidate.monthlyBudgetUsd),
    warningPercent:
      typeof candidate.warningPercent === "number"
      && Number.isFinite(candidate.warningPercent)
        ? Math.min(0.99, Math.max(0.1, candidate.warningPercent))
        : DEFAULT_COST_BUDGET_POLICY.warningPercent,
    enforcement: candidate.enforcement === "pause" ? "pause" : "warn",
  };
}

function sanitizeUsage(value: unknown): CostUsage | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<CostUsage>;
  if (
    !nonNegativeFinite(candidate.promptTokens)
    || !nonNegativeFinite(candidate.completionTokens)
    || !nonNegativeFinite(candidate.totalTokens)
  ) {
    return null;
  }
  return {
    promptTokens: candidate.promptTokens,
    completionTokens: candidate.completionTokens,
    totalTokens: candidate.totalTokens,
  };
}

function sanitizeEntry(value: unknown): CostUsageEntry | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<CostUsageEntry>;
  const usage = sanitizeUsage(candidate.usage);
  if (
    typeof candidate.id !== "string"
    || !candidate.id
    || !nonNegativeFinite(candidate.occurredAtMs)
    || typeof candidate.targetKey !== "string"
    || !candidate.targetKey
    || typeof candidate.targetLabel !== "string"
    || !candidate.targetLabel
    || typeof candidate.sessionId !== "string"
    || !candidate.sessionId
    || !(candidate.runId === null || typeof candidate.runId === "string")
    || !(candidate.costUsd === null || nonNegativeFinite(candidate.costUsd))
    || !usage
  ) {
    return null;
  }
  return {
    id: candidate.id,
    occurredAtMs: candidate.occurredAtMs,
    targetKey: candidate.targetKey,
    targetLabel: candidate.targetLabel,
    sessionId: candidate.sessionId,
    runId: candidate.runId,
    usage,
    costUsd: candidate.costUsd,
    // A malformed or negative latency is dropped to null rather than
    // rejecting the whole entry: the cost accounting it carries is still
    // valid, and an unmeasured latency is already a supported state.
    timeToFirstTokenMs: nonNegativeFinite(candidate.timeToFirstTokenMs)
      ? candidate.timeToFirstTokenMs
      : null,
  };
}

function defaults(): PersistedShape {
  return {
    version: 1,
    policy: { ...DEFAULT_COST_BUDGET_POLICY },
    rates: {},
    entries: [],
  };
}

function sanitizeRates(value: unknown): Record<string, LabCostRate> {
  if (!value || typeof value !== "object") return {};
  const rates: Record<string, LabCostRate> = {};
  for (const [targetKey, rawRate] of Object.entries(
    value as Record<string, unknown>,
  )) {
    if (!targetKey || !rawRate || typeof rawRate !== "object") continue;
    const candidate = rawRate as Partial<LabCostRate>;
    if (
      nonNegativeFinite(candidate.inputPerMillionUsd)
      && nonNegativeFinite(candidate.outputPerMillionUsd)
    ) {
      rates[targetKey] = {
        inputPerMillionUsd: candidate.inputPerMillionUsd,
        outputPerMillionUsd: candidate.outputPerMillionUsd,
      };
    }
  }
  return rates;
}

function hydrate(): PersistedShape {
  const fallback = defaults();
  try {
    const raw = localStorage.getItem(COST_CONTROL_STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return fallback;
    return {
      version: 1,
      policy: sanitizePolicy(parsed.policy),
      rates: sanitizeRates(parsed.rates),
      entries: (Array.isArray(parsed.entries) ? parsed.entries : [])
        .map(sanitizeEntry)
        .filter((entry): entry is CostUsageEntry => entry !== null)
        .slice(-MAX_COST_USAGE_ENTRIES),
    };
  } catch {
    return fallback;
  }
}

function persist(
  state: Pick<CostControlState, "policy" | "rates" | "entries">,
): void {
  try {
    localStorage.setItem(
      COST_CONTROL_STORAGE_KEY,
      JSON.stringify({
        version: 1,
        policy: state.policy,
        rates: state.rates,
        entries: state.entries.slice(-MAX_COST_USAGE_ENTRIES),
      } satisfies PersistedShape),
    );
  } catch {
    // Usage accounting must never make a model call fail because storage is full.
  }
}

export function calculateUsageCostUsd(
  rate: LabCostRate | undefined,
  usage: CostUsage,
): number | null {
  if (!rate) return null;
  const cost =
    (usage.promptTokens * rate.inputPerMillionUsd
      + usage.completionTokens * rate.outputPerMillionUsd)
    / 1_000_000;
  return Number.isFinite(cost) && cost >= 0 ? cost : null;
}

function localDayStart(nowMs: number): number {
  const date = new Date(nowMs);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function localMonthStart(nowMs: number): number {
  const date = new Date(nowMs);
  date.setHours(0, 0, 0, 0);
  date.setDate(1);
  return date.getTime();
}

export function summarizeCostWindow(
  entries: readonly CostUsageEntry[],
  fromMs: number,
  nowMs: number,
): CostWindow {
  let spentUsd = 0;
  let unknownCalls = 0;
  let knownCalls = 0;
  for (const entry of entries) {
    if (entry.occurredAtMs < fromMs || entry.occurredAtMs > nowMs) continue;
    if (entry.costUsd === null) {
      unknownCalls += 1;
    } else {
      knownCalls += 1;
      spentUsd += entry.costUsd;
    }
  }
  return { spentUsd, unknownCalls, knownCalls };
}

export function evaluateCostBudget(
  policy: CostBudgetPolicy,
  entries: readonly CostUsageEntry[],
  nowMs = Date.now(),
): CostBudgetEvaluation {
  const daily = summarizeCostWindow(entries, localDayStart(nowMs), nowMs);
  const monthly = summarizeCostWindow(entries, localMonthStart(nowMs), nowMs);
  if (!policy.enabled) {
    return {
      status: "disabled",
      daily,
      monthly,
      exceededWindows: [],
      warningWindows: [],
    };
  }

  const exceededWindows: Array<"daily" | "monthly"> = [];
  const warningWindows: Array<"daily" | "monthly"> = [];
  const inspect = (
    window: "daily" | "monthly",
    spent: number,
    limit: number | null,
  ) => {
    if (limit === null) return;
    if (spent >= limit) exceededWindows.push(window);
    else if (spent >= limit * policy.warningPercent) warningWindows.push(window);
  };
  inspect("daily", daily.spentUsd, policy.dailyBudgetUsd);
  inspect("monthly", monthly.spentUsd, policy.monthlyBudgetUsd);

  return {
    status:
      exceededWindows.length > 0
        ? "exceeded"
        : warningWindows.length > 0
          ? "warning"
          : "ok",
    daily,
    monthly,
    exceededWindows,
    warningWindows,
  };
}

export class CostBudgetExceededError extends Error {
  constructor(readonly evaluation: CostBudgetEvaluation) {
    const windows = evaluation.exceededWindows.join(" and ");
    super(
      `Cloud request paused: the configured ${windows} cost budget has been reached. Adjust the budget or switch enforcement to warn-only in Settings → Usage.`,
    );
    this.name = "CostBudgetExceededError";
  }
}

export function assertCostBudgetAllowsRequest(
  state: Pick<CostControlState, "policy" | "entries">,
  nowMs = Date.now(),
): CostBudgetEvaluation {
  const evaluation = evaluateCostBudget(state.policy, state.entries, nowMs);
  if (
    state.policy.enabled
    && state.policy.enforcement === "pause"
    && evaluation.status === "exceeded"
  ) {
    throw new CostBudgetExceededError(evaluation);
  }
  return evaluation;
}

const initial = hydrate();

export const useCostControlStore = create<CostControlState>((set, get) => ({
  policy: initial.policy,
  rates: initial.rates,
  entries: initial.entries,

  setPolicy: (patch) => {
    set((state) => ({
      policy: sanitizePolicy({ ...state.policy, ...patch }),
    }));
    persist(get());
  },

  setRate: (targetKey, rate) => {
    set((state) => {
      const rates = { ...state.rates };
      if (
        rate === null
        || !nonNegativeFinite(rate.inputPerMillionUsd)
        || !nonNegativeFinite(rate.outputPerMillionUsd)
      ) {
        delete rates[targetKey];
      } else {
        rates[targetKey] = {
          inputPerMillionUsd: rate.inputPerMillionUsd,
          outputPerMillionUsd: rate.outputPerMillionUsd,
        };
      }
      return { rates };
    });
    persist(get());
  },

  recordUsage: (input) => {
    const entry: CostUsageEntry = {
      ...input,
      id: crypto.randomUUID(),
    };
    set((state) => ({
      entries: [...state.entries, entry].slice(-MAX_COST_USAGE_ENTRIES),
    }));
    persist(get());
    return entry;
  },

  clearUsage: () => {
    set({ entries: [] });
    persist(get());
  },
}));

export default useCostControlStore;
