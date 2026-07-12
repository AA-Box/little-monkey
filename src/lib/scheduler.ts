/**
 * In-app cron scheduler (design doc: docs/roadmap/p3-scheduled-automation.md,
 * slice 3): a 30s tick loop, started once from `App.tsx`'s boot effect on
 * the main window only (see `startScheduler`'s caller), that runs any
 * enabled `AutomationEntry` whose cron expression had an occurrence since
 * the last tick.
 */
import { invoke } from "@tauri-apps/api/core";

import { useAutomationsStore, type AutomationEntry } from "../store/automationsStore";
import { useRecipeStore } from "../store/recipeStore";
import { useSessionStore } from "../store/sessionStore";
import { runRecipeNow } from "./recipeRunner";

const TICK_INTERVAL_MS = 30_000;

let tickTimer: ReturnType<typeof setInterval> | null = null;
/** Updated at the end of every tick — an entry is "due" when its most recent
 * cron occurrence falls after this, i.e. an occurrence was crossed since we
 * last looked (see `isEntryDue`). */
let lastCheckedAtMs = Date.now();
/** Entries currently mid-run — single-flight per entry so a slow run can't
 * be started twice by back-to-back ticks. */
const inFlight = new Set<string>();

/**
 * Whether `entry` has had a cron occurrence since `checkedSinceMs` (the tick
 * loop's shared `lastCheckedAtMs`, passed explicitly so this stays a pure,
 * directly-testable function instead of reading module-level mutable state).
 * `catchUpIfMissed` compares against the entry's own `lastRunAt` instead, so
 * a schedule missed entirely while the app was closed still fires once on
 * next launch rather than silently skipping to the next occurrence.
 */
export async function isEntryDue(entry: AutomationEntry, checkedSinceMs: number): Promise<boolean> {
  try {
    const previous = await invoke<number>("cron_previous", { expr: entry.cron });
    if (entry.catchUpIfMissed) {
      return previous > (entry.lastRunAt ?? 0);
    }
    return previous > checkedSinceMs;
  } catch {
    // A cron expression that fails to parse (e.g. hand-edited automations.json)
    // never fires rather than crashing the tick loop for every other entry.
    return false;
  }
}

async function runEntry(entry: AutomationEntry): Promise<void> {
  const recipe = useRecipeStore.getState().recipes.find((r) => r.recipe?.name === entry.recipeName)?.recipe;
  if (!recipe) {
    useAutomationsStore.getState().recordRun(entry.id, "error");
    return;
  }
  try {
    const { sessionId, done } = await runRecipeNow(recipe, {}, entry.permissionModeOverride);
    await done;
    useAutomationsStore.getState().recordRun(entry.id, "ok", sessionId);
  } catch {
    useAutomationsStore.getState().recordRun(entry.id, "error");
  }
}

async function tick(): Promise<void> {
  const now = Date.now();
  // Skip the whole tick while ANY turn is running anywhere in this window —
  // a scheduled run must never start while a split-pane (or another
  // scheduled) turn is already streaming, since both would otherwise fight
  // over the global permission mode (see `recipeRunner.ts`'s own doc comment
  // on that limitation).
  if (Object.keys(useSessionStore.getState().runningTurns).length > 0) {
    lastCheckedAtMs = now;
    return;
  }

  const dueChecks = useAutomationsStore
    .getState()
    .entries.filter((entry) => entry.enabled && !inFlight.has(entry.id));

  for (const entry of dueChecks) {
    if (!(await isEntryDue(entry, lastCheckedAtMs))) continue;
    inFlight.add(entry.id);
    void runEntry(entry).finally(() => inFlight.delete(entry.id));
    // Only one scheduled run starts per tick — the next tick's "any turn
    // running" check above then naturally holds off starting a second one
    // until this one finishes.
    break;
  }
  lastCheckedAtMs = now;
}

let started = false;

/** Starts the 30s tick loop — idempotent (safe to call more than once; only
 * the first call does anything). Callers are responsible for only invoking
 * this on the main window (see `App.tsx`) — every other window would
 * otherwise run its own independent scheduler against the same shared
 * `automations.json`. */
export function startScheduler(): void {
  if (started) return;
  started = true;
  lastCheckedAtMs = Date.now();
  tickTimer = setInterval(() => void tick(), TICK_INTERVAL_MS);
}

/** Stops the tick loop — exposed mainly for tests; the running app never
 * calls this itself. */
export function stopScheduler(): void {
  if (tickTimer !== null) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
  started = false;
}
