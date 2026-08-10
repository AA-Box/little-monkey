import { useEffect, useMemo, useRef, useState } from "react";
import { Cpu } from "lucide-react";
import { Button } from "../ui";
import { useUsageHistoryStore } from "../../store/usageHistoryStore";
import { useSessionStore } from "../../store/sessionStore";
import {
  compareBillingForMonth,
  evaluateCostBudget,
  monthKey,
  sanitizeWarningPercents,
  useCostControlStore,
  type CostAttributionScope,
} from "../../store/costControlStore";
import { useResourceLedgerStore } from "../../store/resourceLedgerStore";
import { useModelStore } from "../../store/modelStore";
import { providerModelTargetKey } from "../../lib/modelTargets";
import { useT } from "../../lib/i18n";
import { formatDuration } from "../../lib/format";
import { renderTotal, type UsageTotalField } from "../../lib/processUsage";
import {
  accountByWorkspace,
  workspaceDisplayName,
} from "../../lib/workspaceAccounting";

/** A full trailing year, matching the "Aug ... Jul" span of a GitHub-style contribution graph. */
const WEEKS = 52;
const DAYS_PER_WEEK = 7;

type ActivityMode = "daily" | "weekly" | "cumulative";

/** Local calendar date key ("YYYY-MM-DD") — must match `usageHistoryStore.ts`'s `todayKey()` bucketing. */
function dateKey(d: Date): string {
  const year = d.getFullYear();
  const month = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function formatTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(n >= 10_000_000_000 ? 0 : 1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 10_000 ? 0 : 1)}K`;
  return String(n);
}

/** "13h 16m" / "9m" / "42s" — mirrors how long a single `runAgentTurn` (one full user message, including every tool round-trip) took wall-clock. */

/** Consecutive-calendar-day run lengths computed straight off real recorded
 * days — never estimated. `current` counts back from today (or yesterday, so
 * a streak isn't reset to 0 the instant the clock rolls into a new day
 * before that day's first turn). */
function computeStreaks(dailyTotals: Record<string, number>, today: Date): { current: number; longest: number } {
  const activeDays = new Set(Object.keys(dailyTotals).filter((key) => (dailyTotals[key] ?? 0) > 0));
  if (activeDays.size === 0) return { current: 0, longest: 0 };

  const sorted = Array.from(activeDays).sort();
  let longest = 1;
  let run = 1;
  for (let i = 1; i < sorted.length; i++) {
    const prev = new Date(`${sorted[i - 1]}T00:00:00`);
    const cur = new Date(`${sorted[i]}T00:00:00`);
    const diffDays = Math.round((cur.getTime() - prev.getTime()) / 86_400_000);
    run = diffDays === 1 ? run + 1 : 1;
    longest = Math.max(longest, run);
  }

  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  let anchor: Date | null = null;
  if (activeDays.has(dateKey(today))) anchor = today;
  else if (activeDays.has(dateKey(yesterday))) anchor = yesterday;

  let current = 0;
  if (anchor) {
    current = 1;
    const cursor = new Date(anchor);
    for (;;) {
      cursor.setDate(cursor.getDate() - 1);
      if (!activeDays.has(dateKey(cursor))) break;
      current += 1;
    }
  }

  return { current, longest };
}

function StatCell({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex-1 px-2 py-4 text-center">
      <p className="text-xl font-semibold text-foreground">{value}</p>
      <p className="mt-0.5 text-xs text-muted">{label}</p>
    </div>
  );
}

/** Cycled per row so the model list reads as visually distinct at a glance,
 * the way the reference dashboard's per-plugin brand icons do — there's no
 * per-model brand icon to draw here, so a rotating color stands in for one. */
const MODEL_ICON_COLORS = ["bg-orange-500", "bg-violet-500", "bg-teal-500", "bg-rose-500", "bg-blue-500", "bg-amber-500"];

type HeatmapCell = { date: Date; key: string; tokens: number; cumulative: number; future: boolean };

/** 0 (no activity) through 4 (near the busiest cell in the visible window/mode). */
function levelFor(value: number, max: number): 0 | 1 | 2 | 3 | 4 {
  if (value <= 0) return 0;
  const ratio = value / max;
  if (ratio > 0.75) return 4;
  if (ratio > 0.5) return 3;
  if (ratio > 0.25) return 2;
  return 1;
}

const LEVEL_CLASSES = ["bg-surface-2", "bg-accent/25", "bg-accent/50", "bg-accent/75", "bg-accent"];

const MODES: ActivityMode[] = ["daily", "weekly", "cumulative"];

const ATTRIBUTION_SCOPES: CostAttributionScope[] = ["workspace", "project", "session", "target"];

/** The device measurements shown beside a workspace's token bill. Wall and CPU
 * time answer "how long did this workspace hold the machine"; GPU device time
 * is the one that a token bill can never stand in for, since a local model's
 * whole cost is device time. */
const DEVICE_FIELDS: UsageTotalField[] = ["wallTimeMs", "cpuTimeMs", "gpuDeviceMs"];

function usd(value: number): string {
  return `$${value.toFixed(4)}`;
}

function percent(fraction: number): string {
  return `${Math.round(fraction * 100)}%`;
}

/**
 * Settings "Usage" tab: lifetime/peak/longest-task/streak stats, a
 * GitHub-style daily activity heatmap over the trailing year (with
 * daily/weekly/cumulative views), a "most used models" list, and an activity
 * insights list — all read from `usageHistoryStore.ts`, which every real
 * turn, tool call, subagent run, and verify run accumulates into as it
 * happens. No backend involved, and nothing here is estimated: nothing
 * appears until the app has actually done it.
 */
export function UsagePanel() {
  const { t } = useT();
  const totalTokens = useUsageHistoryStore((s) => s.totalTokens);
  const peakTurnTokens = useUsageHistoryStore((s) => s.peakTurnTokens);
  const dailyTotals = useUsageHistoryStore((s) => s.dailyTotals);
  const byModel = useUsageHistoryStore((s) => s.byModel);
  const totalTurns = useUsageHistoryStore((s) => s.totalTurns);
  const longestTurnMs = useUsageHistoryStore((s) => s.longestTurnMs);
  const toolCallsMade = useUsageHistoryStore((s) => s.toolCallsMade);
  const subagentTasksRun = useUsageHistoryStore((s) => s.subagentTasksRun);
  const verifyRuns = useUsageHistoryStore((s) => s.verifyRuns);
  const clear = useUsageHistoryStore((s) => s.clear);
  const costPolicy = useCostControlStore((s) => s.policy);
  const costRates = useCostControlStore((s) => s.rates);
  const costEntries = useCostControlStore((s) => s.entries);
  const costReconciliations = useCostControlStore((s) => s.reconciliations);
  const setCostPolicy = useCostControlStore((s) => s.setPolicy);
  const setCostRate = useCostControlStore((s) => s.setRate);
  const setReconciliation = useCostControlStore((s) => s.setReconciliation);
  const clearCostUsage = useCostControlStore((s) => s.clearUsage);
  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  const chatSessions = useSessionStore((s) => s.sessions.length);
  const sessions = useSessionStore((s) => s.sessions);
  const ledgerRows = useResourceLedgerStore((s) => s.rows);
  const refreshLedger = useResourceLedgerStore((s) => s.refreshLedger);
  const ledgerError = useResourceLedgerStore((s) => s.ledgerError);

  const [confirmingClear, setConfirmingClear] = useState(false);
  const [mode, setMode] = useState<ActivityMode>("daily");
  const [selectedCostTarget, setSelectedCostTarget] = useState("");
  const [attributionScope, setAttributionScope] = useState<CostAttributionScope>("workspace");
  // Held as text so a half-typed list ("50, 8") is not rewritten under the
  // cursor by the sanitizer; committed on blur.
  const [tiersDraft, setTiersDraft] = useState<string | null>(null);

  // Read once when the tab opens. The resource ledger is not a live dashboard
  // (see `resourceLedgerStore.ts`) — it is read when somebody asks what
  // something cost, which is exactly what opening this tab is.
  useEffect(() => {
    void refreshLedger();
  }, [refreshLedger]);
  const gridWrapRef = useRef<HTMLDivElement>(null);
  const [cellHover, setCellHover] = useState<{ x: number; y: number; label: string } | null>(null);

  const today = useMemo(() => {
    const d = new Date();
    d.setHours(0, 0, 0, 0);
    return d;
  }, []);

  const { current: currentStreak, longest: longestStreak } = useMemo(
    () => computeStreaks(dailyTotals, today),
    [dailyTotals, today],
  );

  const grid = useMemo(() => {
    const totalDays = WEEKS * DAYS_PER_WEEK;
    // Weeks run Sunday -> Saturday, with the last column's Saturday clamped to today.
    const gridEnd = new Date(today);
    gridEnd.setDate(gridEnd.getDate() + (DAYS_PER_WEEK - 1 - today.getDay()));
    const gridStart = new Date(gridEnd);
    gridStart.setDate(gridStart.getDate() - (totalDays - 1));

    const cells: HeatmapCell[] = [];
    let running = 0;
    for (let i = 0; i < totalDays; i++) {
      const date = new Date(gridStart);
      date.setDate(date.getDate() + i);
      const key = dateKey(date);
      const tokens = dailyTotals[key] ?? 0;
      running += tokens;
      cells.push({ date, key, tokens, cumulative: running, future: date > today });
    }

    const weeks: HeatmapCell[][] = [];
    for (let w = 0; w < WEEKS; w++) weeks.push(cells.slice(w * DAYS_PER_WEEK, (w + 1) * DAYS_PER_WEEK));
    const weekTotals = weeks.map((week) => week.reduce((sum, cell) => sum + cell.tokens, 0));

    // Labeled one column BEFORE the month actually changes (index `i - 1`,
    // not `i`) — reads better against the grid above it than anchoring to
    // the new month's first column. Never writes index 0: a 52-week window
    // is ~11.97 months, so unless it happens to land exactly on a month
    // boundary, the first and last columns fall in the same calendar month
    // one year apart, and labeling off the very first transition would show
    // that month's name twice (13 labels for what should read as 12).
    // Feb and Mar are exempted from that back-shift (label stays at the
    // transition column `i` itself) — the natural leap-year/28-vs-31-day
    // drift lands them one column later than every other month if shifted
    // the same way as the rest.
    const monthLabels = new Array(WEEKS).fill("");
    for (let i = 1; i < WEEKS; i++) {
      const first = weeks[i][0].date;
      const prev = weeks[i - 1][0].date;
      if (first.getMonth() !== prev.getMonth()) {
        const isFebOrMar = first.getMonth() === 1 || first.getMonth() === 2;
        monthLabels[isFebOrMar ? i : i - 1] = first.toLocaleDateString(undefined, { month: "short" });
      }
    }

    return {
      weeks,
      weekTotals,
      monthLabels,
      dailyMax: Math.max(1, ...cells.map((c) => c.tokens)),
      weeklyMax: Math.max(1, ...weekTotals),
      cumulativeMax: Math.max(1, running),
    };
  }, [dailyTotals, today]);

  function valueFor(cell: HeatmapCell, weekIndex: number): number {
    if (mode === "weekly") return grid.weekTotals[weekIndex];
    if (mode === "cumulative") return cell.cumulative;
    return cell.tokens;
  }
  const maxForMode = mode === "weekly" ? grid.weeklyMax : mode === "cumulative" ? grid.cumulativeMax : grid.dailyMax;

  const MAX_MODEL_ROWS = 5;
  const modelRows = useMemo(
    () => Object.entries(byModel).sort(([, a], [, b]) => b.totalTokens - a.totalTokens).slice(0, MAX_MODEL_ROWS),
    [byModel],
  );

  const providerTargets = useMemo(
    () =>
      providers.flatMap((provider) =>
        (providerModels[provider.id] ?? []).map((model) => ({
          key: providerModelTargetKey(provider.id, model.id),
          label: `${provider.label} · ${model.id}`,
        })),
      ),
    [providerModels, providers],
  );
  const activeCostTarget =
    providerTargets.find((target) => target.key === selectedCostTarget)
    ?? providerTargets[0]
    ?? null;
  const activeCostRate = activeCostTarget ? costRates[activeCostTarget.key] : undefined;
  const costEvaluation = useMemo(
    () => evaluateCostBudget(costPolicy, costEntries),
    [costEntries, costPolicy],
  );

  const currentMonth = useMemo(() => monthKey(Date.now()), []);

  const attributionRows = useMemo(
    () => accountByWorkspace(costEntries, ledgerRows, attributionScope),
    [attributionScope, costEntries, ledgerRows],
  );

  const sessionTitles = useMemo(
    () => new Map(sessions.map((session) => [session.id, session.title])),
    [sessions],
  );

  const billingRows = useMemo(
    () => compareBillingForMonth(costEntries, costReconciliations, currentMonth),
    [costEntries, costReconciliations, currentMonth],
  );

  /** Every provider with a key configured, so a month can be reconciled even
   * before the app has recorded a single priced call against it. */
  const reconcilableProviders = useMemo(() => {
    const ids = new Set(billingRows.map((row) => row.providerId));
    for (const provider of providers) if (provider.has_key) ids.add(provider.id);
    return Array.from(ids).sort();
  }, [billingRows, providers]);

  function attributionLabel(key: string): string {
    if (key === "") return t("UsagePanel.unattributed");
    if (attributionScope === "session") return sessionTitles.get(key) ?? key;
    if (attributionScope === "target") {
      return costEntries.find((entry) => entry.targetKey === key)?.targetLabel ?? key;
    }
    return workspaceDisplayName(key);
  }

  function handleClear() {
    clear();
    clearCostUsage();
    setConfirmingClear(false);
  }

  const insights: [string, number][] = [
    [t("UsagePanel.insightTotalTurns"), totalTurns],
    [t("UsagePanel.insightToolCalls"), toolCallsMade],
    [t("UsagePanel.insightSubagentTasks"), subagentTasksRun],
    [t("UsagePanel.insightVerifyRuns"), verifyRuns],
    [t("UsagePanel.insightChatSessions"), chatSessions],
  ];

  return (
    <div className="flex flex-col gap-6 py-2">
      <p className="text-xs text-muted">{t("UsagePanel.description")}</p>

      <div className="flex divide-x divide-border rounded-xl border border-border">
        <StatCell label={t("UsagePanel.statLifetime")} value={formatTokens(totalTokens)} />
        <StatCell label={t("UsagePanel.statPeak")} value={formatTokens(peakTurnTokens)} />
        <StatCell label={t("UsagePanel.statLongestTask")} value={formatDuration(longestTurnMs, { style: "coarse", fallback: "0m" })} />
        <StatCell label={t("UsagePanel.statCurrentStreak")} value={t("UsagePanel.dayCount", { count: currentStreak })} />
        <StatCell label={t("UsagePanel.statLongestStreak")} value={t("UsagePanel.dayCount", { count: longestStreak })} />
      </div>

      <section className="rounded-xl border border-border bg-surface-1 p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h3 className="text-sm font-semibold text-foreground">
              {t("UsagePanel.costControlsHeading")}
            </h3>
            <p className="mt-1 max-w-2xl text-xs leading-relaxed text-muted">
              {t("UsagePanel.costControlsDescription")}
            </p>
          </div>
          <label className="flex items-center gap-2 text-xs font-medium text-foreground">
            <input
              type="checkbox"
              checked={costPolicy.enabled}
              onChange={(event) => setCostPolicy({ enabled: event.target.checked })}
              className="accent-accent"
            />
            {t("UsagePanel.costControlsEnabled")}
          </label>
        </div>

        <div className="mt-4 grid grid-cols-1 gap-3 sm:grid-cols-4">
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("UsagePanel.dailyBudget")}</span>
            <input
              type="number"
              min="0"
              step="0.01"
              value={costPolicy.dailyBudgetUsd ?? ""}
              onChange={(event) =>
                setCostPolicy({
                  dailyBudgetUsd:
                    event.target.value === "" ? null : Number(event.target.value),
                })
              }
              className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
            />
          </label>
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("UsagePanel.monthlyBudget")}</span>
            <input
              type="number"
              min="0"
              step="0.01"
              value={costPolicy.monthlyBudgetUsd ?? ""}
              onChange={(event) =>
                setCostPolicy({
                  monthlyBudgetUsd:
                    event.target.value === "" ? null : Number(event.target.value),
                })
              }
              className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
            />
          </label>
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("UsagePanel.warningThresholds")}</span>
            <input
              type="text"
              inputMode="numeric"
              value={
                tiersDraft
                ?? costPolicy.warningPercents.map((tier) => Math.round(tier * 100)).join(", ")
              }
              onChange={(event) => setTiersDraft(event.target.value)}
              onBlur={(event) => {
                setCostPolicy({
                  warningPercents: sanitizeWarningPercents(
                    event.target.value
                      .split(",")
                      .map((part) => Number(part.trim()) / 100)
                      .filter((value) => Number.isFinite(value) && value > 0),
                  ),
                });
                // Dropped so the field re-renders from the sanitized policy —
                // the user sees exactly the tiers that will be enforced, not
                // the text they typed.
                setTiersDraft(null);
              }}
              className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
            />
          </label>
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("UsagePanel.enforcement")}</span>
            <select
              value={costPolicy.enforcement}
              onChange={(event) =>
                setCostPolicy({
                  enforcement: event.target.value === "pause" ? "pause" : "warn",
                })
              }
              className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
            >
              <option value="warn">{t("UsagePanel.enforcementWarn")}</option>
              <option value="pause">{t("UsagePanel.enforcementPause")}</option>
            </select>
          </label>
        </div>

        <div className="mt-4 grid grid-cols-1 gap-3 sm:grid-cols-3">
          <div className="rounded-lg border border-border px-3 py-2">
            <p className="text-[11px] uppercase tracking-wide text-faint">
              {t("UsagePanel.todaySpend")}
            </p>
            <p className="mt-1 text-sm font-semibold text-foreground">
              ${costEvaluation.daily.spentUsd.toFixed(4)}
            </p>
          </div>
          <div className="rounded-lg border border-border px-3 py-2">
            <p className="text-[11px] uppercase tracking-wide text-faint">
              {t("UsagePanel.monthSpend")}
            </p>
            <p className="mt-1 text-sm font-semibold text-foreground">
              ${costEvaluation.monthly.spentUsd.toFixed(4)}
            </p>
          </div>
          <div className="rounded-lg border border-border px-3 py-2">
            <p className="text-[11px] uppercase tracking-wide text-faint">
              {t("UsagePanel.accountingStatus")}
            </p>
            <p
              className={`mt-1 text-sm font-semibold ${
                costEvaluation.status === "exceeded"
                  ? "text-danger"
                  : costEvaluation.status === "warning"
                    ? "text-warning"
                    : "text-foreground"
              }`}
            >
              {t(`UsagePanel.costStatus.${costEvaluation.status}`)}
            </p>
            {costEvaluation.highestTier !== null && (
              <p className="mt-1 text-[11px] text-muted">
                {t("UsagePanel.tierCrossed", {
                  percent: percent(costEvaluation.highestTier),
                })}
              </p>
            )}
            {costEvaluation.monthly.unknownCalls > 0 && (
              <p className="mt-1 text-[11px] text-warning">
                {t("UsagePanel.unknownPricedCalls", {
                  count: costEvaluation.monthly.unknownCalls,
                })}
              </p>
            )}
          </div>
        </div>

        <div className="mt-4 border-t border-border pt-4">
          <h4 className="text-xs font-semibold text-foreground">
            {t("UsagePanel.pricingHeading")}
          </h4>
          <p className="mt-1 text-[11px] text-faint">
            {t("UsagePanel.pricingDescription")}
          </p>
          {activeCostTarget ? (
            <div className="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
              <label className="text-xs text-muted">
                <span className="mb-1 block">{t("UsagePanel.model")}</span>
                <select
                  value={activeCostTarget.key}
                  onChange={(event) => setSelectedCostTarget(event.target.value)}
                  className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
                >
                  {providerTargets.map((target) => (
                    <option key={target.key} value={target.key}>
                      {target.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-xs text-muted">
                <span className="mb-1 block">{t("UsagePanel.inputPrice")}</span>
                <input
                  type="number"
                  min="0"
                  step="0.01"
                  value={activeCostRate?.inputPerMillionUsd ?? ""}
                  onChange={(event) => {
                    const value = event.target.value;
                    if (value === "" && activeCostRate === undefined) return;
                    setCostRate(activeCostTarget.key, {
                      inputPerMillionUsd: value === "" ? 0 : Number(value),
                      outputPerMillionUsd: activeCostRate?.outputPerMillionUsd ?? 0,
                    });
                  }}
                  className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
                />
              </label>
              <label className="text-xs text-muted">
                <span className="mb-1 block">{t("UsagePanel.outputPrice")}</span>
                <input
                  type="number"
                  min="0"
                  step="0.01"
                  value={activeCostRate?.outputPerMillionUsd ?? ""}
                  onChange={(event) => {
                    const value = event.target.value;
                    if (value === "" && activeCostRate === undefined) return;
                    setCostRate(activeCostTarget.key, {
                      inputPerMillionUsd: activeCostRate?.inputPerMillionUsd ?? 0,
                      outputPerMillionUsd: value === "" ? 0 : Number(value),
                    });
                  }}
                  className="w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground"
                />
              </label>
            </div>
          ) : (
            <p className="mt-3 text-xs text-faint">
              {t("UsagePanel.noProviderModels")}
            </p>
          )}
        </div>

        <div className="mt-4 border-t border-border pt-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h4 className="text-xs font-semibold text-foreground">
                {t("UsagePanel.attributionHeading")}
              </h4>
              <p className="mt-1 max-w-2xl text-[11px] leading-relaxed text-faint">
                {t("UsagePanel.attributionDescription")}
              </p>
            </div>
            <div className="flex items-center gap-2.5">
              {ATTRIBUTION_SCOPES.map((scope) => (
                <button
                  key={scope}
                  type="button"
                  onClick={() => setAttributionScope(scope)}
                  className={`text-xs transition-colors ${
                    attributionScope === scope
                      ? "font-medium text-foreground"
                      : "text-faint hover:text-muted"
                  }`}
                >
                  {t(`UsagePanel.scope.${scope}`)}
                </button>
              ))}
            </div>
          </div>

          {attributionScope === "workspace" && ledgerError && (
            // The device half failed to read. Said out loud rather than shown
            // as an empty column, which would read as "this workspace used no
            // device time".
            <p className="mt-2 text-[11px] text-warning">
              {t("UsagePanel.deviceLedgerUnavailable", { reason: ledgerError })}
            </p>
          )}

          {attributionRows.length === 0 ? (
            <p className="mt-3 text-xs text-faint">{t("UsagePanel.attributionEmpty")}</p>
          ) : (
            <div className="mt-3 overflow-x-auto">
              <table className="w-full min-w-[40rem] border-collapse text-left text-xs">
                <thead>
                  <tr className="text-[11px] uppercase tracking-wide text-faint">
                    <th className="py-1.5 pr-3 font-normal">
                      {t(`UsagePanel.scope.${attributionScope}`)}
                    </th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colEstimatedSpend")}
                    </th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colTokens")}
                    </th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colUnpricedCalls")}
                    </th>
                    {attributionScope === "workspace"
                      && DEVICE_FIELDS.map((field) => (
                        <th key={field} className="py-1.5 pr-3 text-right font-normal">
                          {t(`UsagePanel.colDevice_${field}`)}
                        </th>
                      ))}
                  </tr>
                </thead>
                <tbody className="divide-y divide-border">
                  {attributionRows.map((row) => (
                    <tr key={row.key || "unattributed"}>
                      <td className="max-w-[16rem] truncate py-2 pr-3 text-foreground" title={row.key}>
                        {attributionLabel(row.key)}
                      </td>
                      <td className="py-2 pr-3 text-right font-mono text-foreground">
                        {row.cost ? usd(row.cost.spentUsd) : t("UsagePanel.noRecordedCalls")}
                      </td>
                      <td className="py-2 pr-3 text-right font-mono text-muted">
                        {row.cost ? formatTokens(row.cost.totalTokens) : "—"}
                      </td>
                      <td className="py-2 pr-3 text-right font-mono text-muted">
                        {row.cost
                          ? row.cost.unknownCalls > 0
                            ? String(row.cost.unknownCalls)
                            : "0"
                          : "—"}
                      </td>
                      {attributionScope === "workspace"
                        && DEVICE_FIELDS.map((field) => {
                          // Same rule as the resource ledger panel: an
                          // unmeasured total renders as its reason, never as a
                          // zero. A workspace with no process rows at all gets
                          // the "no rows" reason rather than a blank cell.
                          const rendered = row.device
                            ? renderTotal(
                                row.device[field],
                                "duration",
                                t("UsagePanel.deviceUnmeasured"),
                              )
                            : { available: false as const, reason: t("UsagePanel.deviceNoRows") };
                          return (
                            <td key={field} className="py-2 pr-3 text-right">
                              {rendered.available ? (
                                <span className="font-mono text-foreground">{rendered.text}</span>
                              ) : (
                                <span className="text-[11px] text-warning">{rendered.reason}</span>
                              )}
                            </td>
                          );
                        })}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>

        <div className="mt-4 border-t border-border pt-4">
          <h4 className="text-xs font-semibold text-foreground">
            {t("UsagePanel.reconciliationHeading")}
          </h4>
          <p className="mt-1 max-w-2xl text-[11px] leading-relaxed text-faint">
            {t("UsagePanel.reconciliationDescription", { month: currentMonth })}
          </p>
          {reconcilableProviders.length === 0 ? (
            <p className="mt-3 text-xs text-faint">{t("UsagePanel.reconciliationEmpty")}</p>
          ) : (
            <div className="mt-3 overflow-x-auto">
              <table className="w-full min-w-[34rem] border-collapse text-left text-xs">
                <thead>
                  <tr className="text-[11px] uppercase tracking-wide text-faint">
                    <th className="py-1.5 pr-3 font-normal">{t("UsagePanel.colProvider")}</th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colEstimatedSpend")}
                    </th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colActualBilled")}
                    </th>
                    <th className="py-1.5 pr-3 text-right font-normal">
                      {t("UsagePanel.colDrift")}
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-border">
                  {reconcilableProviders.map((providerId) => {
                    const row = billingRows.find((candidate) => candidate.providerId === providerId);
                    const estimated = row?.estimatedUsd ?? 0;
                    const unknownCalls = row?.unknownCalls ?? 0;
                    return (
                      <tr key={providerId}>
                        <td className="py-2 pr-3 text-foreground">
                          {providers.find((provider) => provider.id === providerId)?.label
                            ?? providerId}
                          {unknownCalls > 0 && (
                            <span className="ml-2 text-[11px] text-warning">
                              {t("UsagePanel.unknownPricedCalls", { count: unknownCalls })}
                            </span>
                          )}
                        </td>
                        <td className="py-2 pr-3 text-right font-mono text-muted">
                          {usd(estimated)}
                        </td>
                        <td className="py-2 pr-3 text-right">
                          <input
                            type="number"
                            min="0"
                            step="0.01"
                            placeholder={t("UsagePanel.actualBilledPlaceholder")}
                            value={row?.actualBilledUsd ?? ""}
                            onChange={(event) =>
                              setReconciliation(
                                providerId,
                                currentMonth,
                                event.target.value === "" ? null : Number(event.target.value),
                              )
                            }
                            className="w-28 rounded-md border border-border bg-background px-2 py-1 text-right text-xs text-foreground"
                          />
                        </td>
                        <td className="py-2 pr-3 text-right font-mono">
                          {row?.driftUsd === null || row?.driftUsd === undefined ? (
                            <span className="text-faint">—</span>
                          ) : (
                            <span
                              className={
                                Math.abs(row.driftUsd) > 0.005 ? "text-warning" : "text-muted"
                              }
                            >
                              {row.driftUsd >= 0 ? "+" : "−"}
                              {usd(Math.abs(row.driftUsd))}
                              {row.driftFraction !== null
                                && ` (${percent(Math.abs(row.driftFraction))})`}
                            </span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </section>

      {totalTokens === 0 ? (
        <p className="rounded-lg border border-dashed border-border p-3 text-xs text-faint">{t("UsagePanel.emptyState")}</p>
      ) : (
        <>
          <section>
            <div className="mb-2 flex items-center justify-between">
              <h3 className="text-xs font-medium text-muted">{t("UsagePanel.activityHeading")}</h3>
              <div className="flex items-center gap-2.5">
                {MODES.map((m) => (
                  <button
                    key={m}
                    type="button"
                    onClick={() => setMode(m)}
                    className={`text-xs transition-colors ${mode === m ? "font-medium text-foreground" : "text-faint hover:text-muted"}`}
                  >
                    {t(`UsagePanel.mode.${m}`)}
                  </button>
                ))}
              </div>
            </div>
            {/* The tooltip is a SIBLING of the scrollable grid, both inside
             * this outer (non-scrolling) `relative` wrapper — not a child of
             * the `overflow-x-auto` div below. Setting only `overflow-x`
             * forces the UA to also treat `overflow-y` as `auto` (never
             * `visible`) per the CSS overflow spec, so a tooltip positioned
             * *inside* that scrolling div and popped up above row 1 (or past
             * the right edge, for the last few columns) got silently clipped
             * — read as "goes behind other parts / not readable". Anchoring
             * it here instead means it's never inside a clipping ancestor. */}
            <div ref={gridWrapRef} className="relative">
              <div className="overflow-x-auto">
                {/* CSS grid with `1fr` columns (not flex `w-fit`) so the grid
                 * stretches to fill the panel's actual width instead of
                 * sitting at its fixed pixel content-size with a large blank
                 * gap trailing it on wide windows — `minmax(0, 1fr)` keeps
                 * every column's track width equal regardless of a month
                 * label's text length, the same guarantee the old fixed
                 * per-column width gave against label-driven stretching. */}
                <div className="grid gap-[2px]" style={{ gridTemplateColumns: `repeat(${WEEKS}, minmax(0, 1fr))` }}>
                  {grid.weeks.map((week, wi) => (
                    <div key={wi} className="flex min-w-0 flex-col items-stretch gap-[2px]">
                      {week.map((cell) => (
                        <div
                          key={cell.key}
                          onMouseEnter={(event) => {
                            if (cell.future) return;
                            const wrap = gridWrapRef.current;
                            if (!wrap) return;
                            const cellRect = event.currentTarget.getBoundingClientRect();
                            const wrapRect = wrap.getBoundingClientRect();
                            setCellHover({
                              x: cellRect.left - wrapRect.left + cellRect.width / 2,
                              y: cellRect.top - wrapRect.top,
                              label: `${formatTokens(valueFor(cell, wi))} tokens on ${cell.date.toLocaleDateString(undefined, { month: "short", day: "numeric" })}`,
                            });
                          }}
                          onMouseLeave={() => setCellHover(null)}
                          className={`aspect-square w-full max-w-3 rounded-[2px] ${cell.future ? "bg-transparent" : LEVEL_CLASSES[levelFor(valueFor(cell, wi), maxForMode)]}`}
                        />
                      ))}
                      <span className="block h-3 whitespace-nowrap text-[9px] leading-3 text-faint">{grid.monthLabels[wi]}</span>
                    </div>
                  ))}
                </div>
              </div>

              {cellHover && (
                <div
                  className="pointer-events-none absolute z-20 -translate-x-1/2 -translate-y-[calc(100%+8px)] whitespace-nowrap rounded-lg border border-border bg-background px-2.5 py-1.5 text-xs font-medium text-foreground shadow-lg"
                  style={{
                    left: `clamp(48px, ${cellHover.x}px, calc(100% - 48px))`,
                    top: Math.max(cellHover.y, 32),
                  }}
                >
                  {cellHover.label}
                </div>
              )}
            </div>
          </section>

          <div className="grid grid-cols-1 gap-x-8 gap-y-6 sm:grid-cols-2">
            <section>
              <h3 className="mb-3 text-sm font-semibold text-foreground">{t("UsagePanel.insightsHeading")}</h3>
              <div className="flex flex-col divide-y divide-border">
                {insights.map(([label, value]) => (
                  <div key={label} className="flex items-center justify-between gap-2 py-2.5 text-sm">
                    <span className="text-foreground">{label}</span>
                    <span className="text-muted">{value.toLocaleString()}</span>
                  </div>
                ))}
              </div>
            </section>

            <section>
              <h3 className="mb-3 text-sm font-semibold text-foreground">{t("UsagePanel.byModelHeading")}</h3>
              {modelRows.length === 0 ? (
                <p className="text-xs text-faint">{t("UsagePanel.emptyState")}</p>
              ) : (
                <div className="flex flex-col divide-y divide-border">
                  {modelRows.map(([model, totals], i) => (
                    <div key={model} className="flex items-center gap-2.5 py-2.5 text-sm">
                      <span
                        className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-white ${MODEL_ICON_COLORS[i % MODEL_ICON_COLORS.length]}`}
                      >
                        <Cpu size={13} />
                      </span>
                      <span className="min-w-0 flex-1 truncate text-foreground">{model}</span>
                      <span className="shrink-0 text-muted">{t("UsagePanel.modelTurns", { count: totals.turns })}</span>
                    </div>
                  ))}
                </div>
              )}
            </section>
          </div>

          <div className="flex items-center justify-end gap-2">
            {confirmingClear ? (
              <>
                <span className="text-xs text-muted">{t("UsagePanel.clearConfirmMessage")}</span>
                <Button variant="ghost" size="sm" onClick={() => setConfirmingClear(false)}>
                  {t("UsagePanel.clearCancelButton")}
                </Button>
                <Button variant="danger" size="sm" onClick={handleClear}>
                  {t("UsagePanel.clearConfirmButton")}
                </Button>
              </>
            ) : (
              <Button variant="ghost" size="sm" onClick={() => setConfirmingClear(true)}>
                {t("UsagePanel.clearButton")}
              </Button>
            )}
          </div>
        </>
      )}
    </div>
  );
}

export default UsagePanel;
