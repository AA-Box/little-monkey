import { useMemo, useRef, useState } from "react";
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
    <div className="flex flex-col gap-6 p-2">
      <p className="text-xs text-muted">{t("UsagePanel.description")}</p>

      <div className="flex divide-x divide-border rounded-xl border border-border">
        <StatCell label={t("UsagePanel.statLifetime")} value={formatTokens(totalTokens)} />
        <StatCell label={t("UsagePanel.statPeak")} value={formatTokens(peakTurnTokens)} />
        <StatCell label={t("UsagePanel.statLongestTask")} value={formatDuration(longestTurnMs)} />
        <StatCell label={t("UsagePanel.statCurrentStreak")} value={t("UsagePanel.dayCount", { count: currentStreak })} />
        <StatCell label={t("UsagePanel.statLongestStreak")} value={t("UsagePanel.dayCount", { count: longestStreak })} />
      </div>

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
