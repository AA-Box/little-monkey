/**
 * Synthetic Monitoring Agent (ROADMAP.md Phase 7, item 17) — monitor
 * definitions + run history, plus the in-app scheduled tick loop that
 * drives them. Frontend-only, on purpose: monitor/run persistence uses
 * `localStorage` (the same pattern `workflowDraftStore.ts`/
 * `skillProposalStore.ts` already use for this app's other frontend-owned
 * feature state) rather than a new Tauri command, and the tick loop mirrors
 * `backupScheduler.ts`'s `startXScheduler(): () => void` shape — a plain
 * `setInterval` loop started once from `App.tsx`'s main window, not a new
 * scheduling primitive.
 *
 * The actual journey run (`runMonitorJourney`) and failure diagnosis
 * (`diagnoseMonitorFailure`) live in `../lib/syntheticMonitoring.ts`; this
 * store's only jobs are: own the monitor list + run history, wire the real
 * `resolveTarget`/`attemptStream` closure into the dependency-injected
 * `diagnose` callback (the exact same wiring shape `agentLoop.ts` uses to
 * wire `riskJudge.ts`'s `classifyToolCall`), and drive the scheduled tick.
 */
import { create } from "zustand";
import { isTauri } from "@tauri-apps/api/core";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import {
  createMonitor,
  diagnoseMonitorFailure,
  isMonitorDue,
  runMonitorJourney,
  type MonitorAssertion,
  type MonitorRun,
  type MonitorTargetEnv,
  type SyntheticMonitor,
} from "../lib/syntheticMonitoring";
import { effortForTarget } from "./modelStore";

const MONITORS_STORAGE_KEY = "little-monkey-synthetic-monitors-v1";
const RUNS_STORAGE_KEY = "little-monkey-synthetic-monitor-runs-v1";
/** Capped per monitor so one flaky monitor ticking every minute for weeks
 * doesn't grow `localStorage` without bound — evidence itself lives in the
 * content-addressed artifact store keyed by the ids kept here, so trimming
 * old run rows never deletes the underlying screenshots/logs early. */
const MAX_RUNS_PER_MONITOR = 20;
const TICK_INTERVAL_MS = 30_000;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function persistMonitors(monitors: SyntheticMonitor[]): void {
  try {
    localStorage.setItem(MONITORS_STORAGE_KEY, JSON.stringify({ version: 1, monitors }));
  } catch {
    // Monitors stay live in memory for this session even if localStorage is
    // unavailable or full.
  }
}

function persistRuns(runsByMonitor: Record<string, MonitorRun[]>): void {
  try {
    localStorage.setItem(RUNS_STORAGE_KEY, JSON.stringify({ version: 1, runsByMonitor }));
  } catch {
    // Same best-effort stance as persistMonitors above.
  }
}

function isSyntheticMonitor(value: unknown): value is SyntheticMonitor {
  const item = value as Partial<SyntheticMonitor> | null;
  return Boolean(
    item &&
    typeof item.id === "string" &&
    typeof item.name === "string" &&
    typeof item.url === "string" &&
    typeof item.targetEnv === "string" &&
    typeof item.intervalMinutes === "number" &&
    typeof item.enabled === "boolean" &&
    item.assertion &&
    typeof item.assertion === "object",
  );
}

function isMonitorRun(value: unknown): value is MonitorRun {
  const item = value as Partial<MonitorRun> | null;
  return Boolean(
    item &&
    typeof item.id === "string" &&
    typeof item.monitorId === "string" &&
    typeof item.status === "string" &&
    typeof item.startedAtMs === "number" &&
    item.evidence &&
    typeof item.evidence === "object",
  );
}

function hydrateMonitors(): SyntheticMonitor[] {
  try {
    const raw = JSON.parse(localStorage.getItem(MONITORS_STORAGE_KEY) ?? "null") as { version?: unknown; monitors?: unknown } | null;
    if (raw?.version !== 1 || !Array.isArray(raw.monitors)) return [];
    return raw.monitors.filter(isSyntheticMonitor);
  } catch {
    return [];
  }
}

function hydrateRuns(): Record<string, MonitorRun[]> {
  try {
    const raw = JSON.parse(localStorage.getItem(RUNS_STORAGE_KEY) ?? "null") as { version?: unknown; runsByMonitor?: unknown } | null;
    if (raw?.version !== 1 || !raw.runsByMonitor || typeof raw.runsByMonitor !== "object") return {};
    const entries = Object.entries(raw.runsByMonitor as Record<string, unknown>).map(([monitorId, runs]) => [
      monitorId,
      Array.isArray(runs) ? runs.filter(isMonitorRun) : [],
    ] as const);
    return Object.fromEntries(entries);
  } catch {
    return {};
  }
}

export interface CreateMonitorInput {
  name: string;
  url: string;
  targetEnv: MonitorTargetEnv;
  intervalMinutes: number;
  waitForSelector?: string | null;
  waitForText?: string | null;
  waitTimeoutMs?: number;
  clickSelector?: string | null;
  assertion: MonitorAssertion;
}

interface SyntheticMonitoringState {
  monitors: SyntheticMonitor[];
  runsByMonitor: Record<string, MonitorRun[]>;
  runningMonitorIds: Record<string, boolean>;
  selectedMonitorId: string | null;
  error: string | null;

  selectMonitor: (id: string | null) => void;
  addMonitor: (input: CreateMonitorInput) => SyntheticMonitor;
  updateMonitor: (id: string, input: CreateMonitorInput) => void;
  deleteMonitor: (id: string) => void;
  toggleMonitor: (id: string) => void;
  runMonitorNow: (id: string) => Promise<void>;
  clearError: () => void;
}

/** Wires the store's own `resolveTarget`/`attemptStream` closure into
 * `diagnoseMonitorFailure` — the exact same dependency-injection shape
 * `agentLoop.ts` uses to wire `riskJudge.ts`'s `classifyToolCall` into a
 * turn. `resolveTarget()` is called lazily INSIDE the `callModel` closure
 * (not before) so that if no model is currently active it throws from
 * inside `diagnoseMonitorFailure`'s own try/catch, which fails closed to
 * `null` rather than rejecting the whole monitor run. */
function buildDiagnoseCallback(monitorId: string) {
  return (monitor: SyntheticMonitor, run: MonitorRun, evidenceExcerpt: string, signal?: AbortSignal) =>
    diagnoseMonitorFailure(
      monitor,
      run,
      evidenceExcerpt,
      async (messages, callSignal) => {
        const target = await resolveTarget();
        const effort = effortForTarget(target);
        // `recordUsage: false` — there is no chat session behind
        // `synthetic-monitor:<id>` for this token usage to be attributed
        // to, the same reasoning `subagent.ts`'s child turns use.
        return attemptStream(target, messages, [], callSignal, effort, `synthetic-monitor:${monitorId}`, undefined, false);
      },
      signal,
    );
}

export const useSyntheticMonitoringStore = create<SyntheticMonitoringState>((set, get) => ({
  monitors: hydrateMonitors(),
  runsByMonitor: hydrateRuns(),
  runningMonitorIds: {},
  selectedMonitorId: null,
  error: null,

  selectMonitor: (id) => set({ selectedMonitorId: id }),
  clearError: () => set({ error: null }),

  addMonitor: (input) => {
    const monitor = createMonitor(input);
    const monitors = [monitor, ...get().monitors];
    persistMonitors(monitors);
    set({ monitors, selectedMonitorId: monitor.id, error: null });
    return monitor;
  },

  updateMonitor: (id, input) => {
    const existing = get().monitors.find((entry) => entry.id === id);
    if (!existing) return;
    try {
      const rebuilt = createMonitor({ ...input, now: existing.createdAtMs });
      const updated: SyntheticMonitor = { ...rebuilt, id: existing.id, enabled: existing.enabled, lastRunAtMs: existing.lastRunAtMs };
      const monitors = get().monitors.map((entry) => (entry.id === id ? updated : entry));
      persistMonitors(monitors);
      set({ monitors, error: null });
    } catch (error) {
      set({ error: errorText(error) });
    }
  },

  deleteMonitor: (id) => {
    const monitors = get().monitors.filter((entry) => entry.id !== id);
    const runsByMonitor = { ...get().runsByMonitor };
    delete runsByMonitor[id];
    persistMonitors(monitors);
    persistRuns(runsByMonitor);
    set((state) => ({
      monitors,
      runsByMonitor,
      selectedMonitorId: state.selectedMonitorId === id ? null : state.selectedMonitorId,
    }));
  },

  toggleMonitor: (id) => {
    const monitors = get().monitors.map((entry) => (entry.id === id ? { ...entry, enabled: !entry.enabled } : entry));
    persistMonitors(monitors);
    set({ monitors });
  },

  runMonitorNow: async (id) => {
    const monitor = get().monitors.find((entry) => entry.id === id);
    if (!monitor || get().runningMonitorIds[id]) return;
    set((state) => ({ runningMonitorIds: { ...state.runningMonitorIds, [id]: true } }));
    try {
      const run = await runMonitorJourney(monitor, { diagnose: buildDiagnoseCallback(id) });
      set((state) => {
        const monitors = state.monitors.map((entry) => (entry.id === id ? { ...entry, lastRunAtMs: run.finishedAtMs } : entry));
        const history = [run, ...(state.runsByMonitor[id] ?? [])].slice(0, MAX_RUNS_PER_MONITOR);
        const runsByMonitor = { ...state.runsByMonitor, [id]: history };
        persistMonitors(monitors);
        persistRuns(runsByMonitor);
        return { monitors, runsByMonitor };
      });
    } catch (error) {
      set({ error: errorText(error) });
    } finally {
      set((state) => {
        const runningMonitorIds = { ...state.runningMonitorIds };
        delete runningMonitorIds[id];
        return { runningMonitorIds };
      });
    }
  },
}));

let tickTimer: ReturnType<typeof setInterval> | null = null;

/** One bounded tick: runs at most one due, not-already-running monitor per
 * tick (mirroring `scheduler.ts`'s "one due entry per tick, `break`
 * afterwards" shape) so a burst of simultaneously-due monitors doesn't pile
 * up several disposable browser sessions at once. */
async function tick(): Promise<void> {
  const { monitors, runningMonitorIds, runMonitorNow } = useSyntheticMonitoringStore.getState();
  const now = Date.now();
  for (const monitor of monitors) {
    if (runningMonitorIds[monitor.id]) continue;
    if (!isMonitorDue(monitor, now)) continue;
    await runMonitorNow(monitor.id);
    break;
  }
}

/** Starts the in-app scheduled tick loop and returns a cleanup function —
 * call once from the main window only (see `App.tsx`'s existing
 * `startScheduler`/`startBackupScheduler` call site, which this mirrors).
 * A no-op outside the Tauri shell, since the browser worker this drives
 * only exists there. */
export function startSyntheticMonitoringScheduler(): () => void {
  if (!isTauri()) return () => {};
  void tick().catch((error) => console.error("Synthetic monitoring tick failed", error));
  tickTimer = setInterval(() => {
    void tick().catch((error) => console.error("Synthetic monitoring tick failed", error));
  }, TICK_INTERVAL_MS);
  return () => {
    if (tickTimer !== null) clearInterval(tickTimer);
    tickTimer = null;
  };
}

export function runSyntheticMonitoringTickForTests(): Promise<void> {
  return tick();
}

export default useSyntheticMonitoringStore;
