import { create } from "zustand";

import type { LabCostRate } from "../lib/compareLab";

export const COST_CONTROL_STORAGE_KEY = "little-monkey-cost-controls-v1";
export const MAX_COST_USAGE_ENTRIES = 5_000;

/**
 * The scopes a recorded call can be attributed to (K25).
 *
 * `workspace` and `project` are two different real things in this app, not one
 * thing named twice: the *workspace* is the primary folder open at the moment
 * the request went out, and the *project* is the attached folder the
 * conversation itself belongs to (`session.workspacePath`, snapshotted when the
 * session was created). They differ whenever a chat outlives the folder it was
 * started in — exactly the case where charging today's folder for last week's
 * conversation would be wrong.
 *
 * `workspace` is deliberately the same key the K6 process ledger records in its
 * own `workspace` column (a filesystem path), so a workspace's token bill and
 * its device time join on one identity — see `workspaceAccounting.ts`.
 */
export type CostAttributionScope = "workspace" | "project" | "session" | "target";

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
  /**
   * The primary workspace root path in effect when the request was made, or
   * null when no folder was open. Optional because entries persisted before
   * K25 have no attribution at all — those read back as null, which the
   * surfaces show as "unattributed" rather than folding into some other
   * workspace's bill.
   */
  workspacePath?: string | null;
  /** The attached project folder this conversation belongs to. See
   * {@link CostAttributionScope} for why this is not the same as
   * `workspacePath`. */
  projectPath?: string | null;
}

export type CostBudgetEnforcement = "warn" | "pause";

/**
 * Fractions of a budget that raise a warning, ascending.
 *
 * Three tiers rather than one because a single threshold gives one bit of
 * information for the whole run-up to the limit: at 79% everything is fine and
 * at 81% it is "approaching", with nothing said in between and nothing sharper
 * said at 99%.
 */
export const DEFAULT_WARNING_PERCENTS: readonly number[] = [0.5, 0.8, 0.95];

export const MIN_WARNING_PERCENT = 0.1;
export const MAX_WARNING_PERCENT = 0.99;

export interface CostBudgetPolicy {
  enabled: boolean;
  dailyBudgetUsd: number | null;
  monthlyBudgetUsd: number | null;
  /** Ascending, deduped, each clamped into
   * [{@link MIN_WARNING_PERCENT}, {@link MAX_WARNING_PERCENT}]. Never empty. */
  warningPercents: number[];
  enforcement: CostBudgetEnforcement;
}

/**
 * What the user says a provider actually billed for one calendar month.
 *
 * The app cannot see an invoice and must never imply that it can, so every
 * number it computes is an estimate from rates the user typed in. This record
 * is the one number here that is not an estimate — because a human read it off
 * a bill and entered it — and it exists so the two can be shown side by side
 * with their drift named. Recording one never rewrites the estimate: back-dating
 * a per-call cost from a monthly total would invent a precision no per-call
 * figure has.
 */
export interface BillingReconciliation {
  providerId: string;
  /** Local calendar month, `YYYY-MM`. */
  month: string;
  actualBilledUsd: number;
  recordedAtMs: number;
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
  /** The highest warning fraction each window has crossed, or null for none.
   * A window that has passed the limit outright reports its highest tier here
   * too — "exceeded" is a state, not a reason to forget how it got there. */
  tiers: { daily: number | null; monthly: number | null };
  /** The highest tier crossed in either window — what a status line shows. */
  highestTier: number | null;
}

/** One attribution bucket's share of recorded spend. */
export interface CostAttributionRow {
  /** The bucket's identity: a path, a session id, or a target key. Empty
   * string for entries that carry no attribution for this scope. */
  key: string;
  /** Display label. For an unattributed bucket this is the empty string too —
   * naming it is the caller's job, because only the caller can translate. */
  label: string;
  spentUsd: number;
  knownCalls: number;
  unknownCalls: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

/** One provider's estimated spend for a month beside what it actually billed. */
export interface BillingComparisonRow {
  providerId: string;
  /** Summed from user-entered rates. Always an estimate, never a bill. */
  estimatedUsd: number;
  /** Calls whose target had no rate configured, so they are in no estimate. */
  unknownCalls: number;
  knownCalls: number;
  /** What the user says they were billed, or null when they have not said. */
  actualBilledUsd: number | null;
  /** `actual - estimated`, or null with no actual to compare against. */
  driftUsd: number | null;
  /** `drift / actual`, or null when there is no actual or it is zero — a
   * percentage of zero is not a percentage. */
  driftFraction: number | null;
}

export interface CostControlState {
  policy: CostBudgetPolicy;
  rates: Record<string, LabCostRate>;
  entries: CostUsageEntry[];
  /** Keyed by {@link reconciliationKey}. */
  reconciliations: Record<string, BillingReconciliation>;
  setPolicy: (patch: Partial<CostBudgetPolicy>) => void;
  setRate: (targetKey: string, rate: LabCostRate | null) => void;
  /** Passing null clears the month's entry — "I don't know" is a valid state
   * and must not be spelled as a billed 0. */
  setReconciliation: (
    providerId: string,
    month: string,
    actualBilledUsd: number | null,
    nowMs?: number,
  ) => void;
  recordUsage: (entry: Omit<CostUsageEntry, "id">) => CostUsageEntry;
  clearUsage: () => void;
}

interface PersistedShape {
  version: 2;
  policy: CostBudgetPolicy;
  rates: Record<string, LabCostRate>;
  entries: CostUsageEntry[];
  reconciliations: Record<string, BillingReconciliation>;
}

export const DEFAULT_COST_BUDGET_POLICY: CostBudgetPolicy = {
  enabled: false,
  dailyBudgetUsd: null,
  monthlyBudgetUsd: null,
  warningPercents: [...DEFAULT_WARNING_PERCENTS],
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

/**
 * Ascending, deduped, clamped, never empty.
 *
 * Also accepts the pre-K25 single `warningPercent` so a settings file written
 * by an older build keeps the threshold its owner chose instead of silently
 * reverting to the three defaults.
 */
export function sanitizeWarningPercents(value: unknown, legacy?: unknown): number[] {
  const raw = Array.isArray(value)
    ? value
    : typeof legacy === "number"
      ? [legacy]
      : [];
  const clamped = raw
    .filter((entry): entry is number => typeof entry === "number" && Number.isFinite(entry))
    .map((entry) =>
      Math.min(MAX_WARNING_PERCENT, Math.max(MIN_WARNING_PERCENT, entry)),
    );
  const unique = Array.from(new Set(clamped)).sort((a, b) => a - b);
  return unique.length > 0 ? unique : [...DEFAULT_WARNING_PERCENTS];
}

function sanitizePolicy(value: unknown): CostBudgetPolicy {
  if (!value || typeof value !== "object") return { ...DEFAULT_COST_BUDGET_POLICY };
  const candidate = value as Partial<CostBudgetPolicy> & { warningPercent?: unknown };
  return {
    enabled: candidate.enabled === true,
    dailyBudgetUsd: optionalPositive(candidate.dailyBudgetUsd),
    monthlyBudgetUsd: optionalPositive(candidate.monthlyBudgetUsd),
    warningPercents: sanitizeWarningPercents(
      candidate.warningPercents,
      candidate.warningPercent,
    ),
    enforcement: candidate.enforcement === "pause" ? "pause" : "warn",
  };
}

/** `YYYY-MM` in the local calendar — the same boundary the monthly budget uses. */
export function monthKey(atMs: number): string {
  const date = new Date(atMs);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}`;
}

export function reconciliationKey(providerId: string, month: string): string {
  return `${providerId} ${month}`;
}

const MONTH_PATTERN = /^\d{4}-(0[1-9]|1[0-2])$/;

function sanitizeReconciliations(value: unknown): Record<string, BillingReconciliation> {
  if (!value || typeof value !== "object") return {};
  const out: Record<string, BillingReconciliation> = {};
  for (const raw of Object.values(value as Record<string, unknown>)) {
    if (!raw || typeof raw !== "object") continue;
    const candidate = raw as Partial<BillingReconciliation>;
    if (
      typeof candidate.providerId !== "string"
      || !candidate.providerId
      || typeof candidate.month !== "string"
      || !MONTH_PATTERN.test(candidate.month)
      || !nonNegativeFinite(candidate.actualBilledUsd)
    ) {
      continue;
    }
    out[reconciliationKey(candidate.providerId, candidate.month)] = {
      providerId: candidate.providerId,
      month: candidate.month,
      actualBilledUsd: candidate.actualBilledUsd,
      recordedAtMs: nonNegativeFinite(candidate.recordedAtMs) ? candidate.recordedAtMs : 0,
    };
  }
  return out;
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
    // Same rule as the latency above, for the same reason: an entry written
    // before K25 (or by a window with no folder open) carries valid cost
    // accounting and no attribution, and null is what "not attributed" looks
    // like everywhere downstream.
    workspacePath: typeof candidate.workspacePath === "string" && candidate.workspacePath
      ? candidate.workspacePath
      : null,
    projectPath: typeof candidate.projectPath === "string" && candidate.projectPath
      ? candidate.projectPath
      : null,
  };
}

function defaults(): PersistedShape {
  return {
    version: 2,
    policy: { ...DEFAULT_COST_BUDGET_POLICY },
    rates: {},
    entries: [],
    reconciliations: {},
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
      version: 2,
      policy: sanitizePolicy(parsed.policy),
      rates: sanitizeRates(parsed.rates),
      entries: (Array.isArray(parsed.entries) ? parsed.entries : [])
        .map(sanitizeEntry)
        .filter((entry): entry is CostUsageEntry => entry !== null)
        .slice(-MAX_COST_USAGE_ENTRIES),
      reconciliations: sanitizeReconciliations(parsed.reconciliations),
    };
  } catch {
    return fallback;
  }
}

function persist(
  state: Pick<CostControlState, "policy" | "rates" | "entries" | "reconciliations">,
): void {
  try {
    localStorage.setItem(
      COST_CONTROL_STORAGE_KEY,
      JSON.stringify({
        version: 2,
        policy: state.policy,
        rates: state.rates,
        entries: state.entries.slice(-MAX_COST_USAGE_ENTRIES),
        reconciliations: state.reconciliations,
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
      tiers: { daily: null, monthly: null },
      highestTier: null,
    };
  }

  const exceededWindows: Array<"daily" | "monthly"> = [];
  const warningWindows: Array<"daily" | "monthly"> = [];
  const tiers: { daily: number | null; monthly: number | null } = {
    daily: null,
    monthly: null,
  };
  const inspect = (
    window: "daily" | "monthly",
    spent: number,
    limit: number | null,
  ) => {
    if (limit === null) return;
    // The highest tier the spend has passed, not the first — with tiers at
    // 50/80/95 a run-up to 96% must read as 95%, and taking the first match
    // would report it as 50% for the whole climb.
    const crossed = policy.warningPercents.filter((tier) => spent >= limit * tier);
    tiers[window] = crossed.length > 0 ? crossed[crossed.length - 1] : null;
    if (spent >= limit) exceededWindows.push(window);
    else if (tiers[window] !== null) warningWindows.push(window);
  };
  inspect("daily", daily.spentUsd, policy.dailyBudgetUsd);
  inspect("monthly", monthly.spentUsd, policy.monthlyBudgetUsd);

  const crossedTiers = [tiers.daily, tiers.monthly].filter(
    (tier): tier is number => tier !== null,
  );

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
    tiers,
    highestTier: crossedTiers.length > 0 ? Math.max(...crossedTiers) : null,
  };
}

/** The bucket key an entry falls in for one scope. Empty means unattributed. */
export function attributionKeyOf(
  entry: CostUsageEntry,
  scope: CostAttributionScope,
): string {
  if (scope === "workspace") return entry.workspacePath ?? "";
  if (scope === "project") return entry.projectPath ?? "";
  if (scope === "session") return entry.sessionId;
  return entry.targetKey;
}

/**
 * Recorded spend split by one attribution scope, highest spend first.
 *
 * Entries with nothing recorded for the scope collect into a single bucket
 * whose `key` is the empty string, rather than being dropped or folded into
 * another bucket: a total that quietly excludes what it could not attribute
 * reads as a complete one, which is the same lie the resource ledger's
 * unavailable-with-reason branches exist to avoid.
 */
export function attributeCost(
  entries: readonly CostUsageEntry[],
  scope: CostAttributionScope,
  fromMs = 0,
  nowMs = Date.now(),
): CostAttributionRow[] {
  const buckets = new Map<string, CostAttributionRow>();
  for (const entry of entries) {
    if (entry.occurredAtMs < fromMs || entry.occurredAtMs > nowMs) continue;
    const key = attributionKeyOf(entry, scope);
    let row = buckets.get(key);
    if (!row) {
      row = {
        key,
        label: scope === "target" ? entry.targetLabel : key,
        spentUsd: 0,
        knownCalls: 0,
        unknownCalls: 0,
        promptTokens: 0,
        completionTokens: 0,
        totalTokens: 0,
      };
      buckets.set(key, row);
    }
    if (entry.costUsd === null) row.unknownCalls += 1;
    else {
      row.knownCalls += 1;
      row.spentUsd += entry.costUsd;
    }
    row.promptTokens += entry.usage.promptTokens;
    row.completionTokens += entry.usage.completionTokens;
    row.totalTokens += entry.usage.totalTokens;
  }
  return Array.from(buckets.values()).sort(
    (a, b) => b.spentUsd - a.spentUsd || b.totalTokens - a.totalTokens,
  );
}

/**
 * The provider an entry was billed to, or null for anything that is not a
 * cloud provider call.
 *
 * Reads the key rather than a stored field because `providerModelTargetKey`
 * percent-encodes each part, so the separator is unambiguous and the key is
 * already the canonical identity the rates map is keyed by.
 */
export function providerIdOfTargetKey(targetKey: string): string | null {
  const parts = targetKey.split(":");
  if (parts.length < 3 || parts[0] !== "provider") return null;
  try {
    return decodeURIComponent(parts[1]) || null;
  } catch {
    return null;
  }
}

/**
 * Per-provider estimated spend for one `YYYY-MM`, beside what the user says
 * they were actually billed.
 *
 * Providers with recorded calls and providers with only a recorded invoice both
 * appear: a month where the app estimated nothing but the bill was $40 is the
 * single most useful row this table can show.
 */
export function compareBillingForMonth(
  entries: readonly CostUsageEntry[],
  reconciliations: Readonly<Record<string, BillingReconciliation>>,
  month: string,
): BillingComparisonRow[] {
  const rows = new Map<string, BillingComparisonRow>();
  const rowFor = (providerId: string): BillingComparisonRow => {
    let row = rows.get(providerId);
    if (!row) {
      row = {
        providerId,
        estimatedUsd: 0,
        unknownCalls: 0,
        knownCalls: 0,
        actualBilledUsd: null,
        driftUsd: null,
        driftFraction: null,
      };
      rows.set(providerId, row);
    }
    return row;
  };

  for (const entry of entries) {
    if (monthKey(entry.occurredAtMs) !== month) continue;
    const providerId = providerIdOfTargetKey(entry.targetKey);
    if (!providerId) continue;
    const row = rowFor(providerId);
    if (entry.costUsd === null) row.unknownCalls += 1;
    else {
      row.knownCalls += 1;
      row.estimatedUsd += entry.costUsd;
    }
  }

  for (const record of Object.values(reconciliations)) {
    if (record.month !== month) continue;
    rowFor(record.providerId).actualBilledUsd = record.actualBilledUsd;
  }

  for (const row of rows.values()) {
    if (row.actualBilledUsd === null) continue;
    row.driftUsd = row.actualBilledUsd - row.estimatedUsd;
    row.driftFraction =
      row.actualBilledUsd > 0 ? row.driftUsd / row.actualBilledUsd : null;
  }

  return Array.from(rows.values()).sort((a, b) => {
    const byActual = (b.actualBilledUsd ?? 0) - (a.actualBilledUsd ?? 0);
    return byActual || b.estimatedUsd - a.estimatedUsd;
  });
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
  reconciliations: initial.reconciliations,

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

  setReconciliation: (providerId, month, actualBilledUsd, nowMs = Date.now()) => {
    if (!providerId || !MONTH_PATTERN.test(month)) return;
    set((state) => {
      const reconciliations = { ...state.reconciliations };
      const key = reconciliationKey(providerId, month);
      if (actualBilledUsd === null || !nonNegativeFinite(actualBilledUsd)) {
        delete reconciliations[key];
      } else {
        reconciliations[key] = { providerId, month, actualBilledUsd, recordedAtMs: nowMs };
      }
      return { reconciliations };
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
