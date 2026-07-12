import { useMemo, useState } from "react";
import { Cpu } from "lucide-react";
import { Button } from "../ui";
import { useUsageHistoryStore } from "../../store/usageHistoryStore";
import { useSessionStore } from "../../store/sessionStore";
import { useT } from "../../lib/i18n";

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
function formatDuration(ms: number): string {
  if (ms <= 0) return "0m";
  const totalSeconds = Math.round(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${seconds}s`;
}

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

function StatTile({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border bg-background p-3 text-center">
      <p className="text-lg font-semibold text-foreground">{value}</p>
      <p className="text-xs text-faint">{label}</p>
    </div>
  );
}

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
  const chatSessions = useSessionStore((s) => s.sessions.length);

  const [confirmingClear, setConfirmingClear] = useState(false);
  const [mode, setMode] = useState<ActivityMode>("daily");

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

    const monthLabels = weeks.map((week, i) => {
      const first = week[0].date;
      const prev = i > 0 ? weeks[i - 1][0].date : null;
      return !prev || first.getMonth() !== prev.getMonth() ? first.toLocaleDateString(undefined, { month: "short" }) : "";
    });

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

  const modelRows = useMemo(() => Object.entries(byModel).sort(([, a], [, b]) => b.totalTokens - a.totalTokens), [byModel]);

  function handleClear() {
    clear();
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
    <div className="flex flex-col gap-4 p-2">
      <p className="text-xs text-muted">{t("UsagePanel.description")}</p>

      <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
        <StatTile label={t("UsagePanel.statLifetime")} value={formatTokens(totalTokens)} />
        <StatTile label={t("UsagePanel.statPeak")} value={formatTokens(peakTurnTokens)} />
        <StatTile label={t("UsagePanel.statLongestTask")} value={formatDuration(longestTurnMs)} />
        <StatTile label={t("UsagePanel.statCurrentStreak")} value={t("UsagePanel.dayCount", { count: currentStreak })} />
        <StatTile label={t("UsagePanel.statLongestStreak")} value={t("UsagePanel.dayCount", { count: longestStreak })} />
      </div>

      {totalTokens === 0 ? (
        <p className="rounded-lg border border-dashed border-border p-3 text-xs text-faint">{t("UsagePanel.emptyState")}</p>
      ) : (
        <>
          <section>
            <div className="mb-1 flex items-center justify-between">
              <h3 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("UsagePanel.activityHeading")}</h3>
              <div className="flex items-center gap-3">
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
            <div className="overflow-x-auto rounded-lg border border-border bg-background p-3">
              <div className="flex w-fit gap-[2px]">
                {grid.weeks.map((week, wi) => (
                  <div key={wi} className="flex flex-col gap-[2px]">
                    {week.map((cell) => (
                      <div
                        key={cell.key}
                        title={cell.future ? undefined : `${cell.date.toLocaleDateString()} · ${formatTokens(valueFor(cell, wi))} tokens`}
                        className={`h-2.5 w-2.5 rounded-full ${cell.future ? "opacity-0" : LEVEL_CLASSES[levelFor(valueFor(cell, wi), maxForMode)]}`}
                      />
                    ))}
                    <span className="block h-3 text-[9px] leading-3 text-faint">{grid.monthLabels[wi]}</span>
                  </div>
                ))}
              </div>
            </div>
          </section>

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <section>
              <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("UsagePanel.insightsHeading")}</h3>
              <div className="rounded-lg border border-border bg-background px-3">
                {insights.map(([label, value], i) => (
                  <div
                    key={label}
                    className={`flex items-center justify-between gap-2 py-2 text-sm ${i > 0 ? "border-t border-border" : ""}`}
                  >
                    <span className="text-foreground">{label}</span>
                    <span className="text-muted">{value.toLocaleString()}</span>
                  </div>
                ))}
              </div>
            </section>

            <section>
              <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("UsagePanel.byModelHeading")}</h3>
              <div className="rounded-lg border border-border bg-background px-3">
                {modelRows.length === 0 ? (
                  <p className="py-2 text-xs text-faint">{t("UsagePanel.emptyState")}</p>
                ) : (
                  modelRows.map(([model, totals], i) => (
                    <div
                      key={model}
                      className={`flex items-center gap-2 py-2 text-sm ${i > 0 ? "border-t border-border" : ""}`}
                    >
                      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
                        <Cpu size={13} />
                      </span>
                      <span className="min-w-0 flex-1 truncate text-foreground">{model}</span>
                      <span className="shrink-0 text-xs text-faint">{t("UsagePanel.modelTurns", { count: totals.turns })}</span>
                      <span className="shrink-0 text-muted">{formatTokens(totals.totalTokens)}</span>
                    </div>
                  ))
                )}
              </div>
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
