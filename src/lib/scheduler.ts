/**
 * Recipe schedule authority coordinator.
 *
 * The explicitly installed daemon owns the complete schedule set, even when
 * its service is temporarily stopped. The legacy webview timer is used only
 * after a successful status check proves that the daemon is not installed.
 * Unknown/error state fails closed so the same occurrence can never be run by
 * both authorities.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { useAutomationsStore, type AutomationEntry } from "../store/automationsStore";
import { useRecipeStore, type DiscoveredRecipe } from "../store/recipeStore";
import { useSessionStore } from "../store/sessionStore";
import {
  recipeSchedulerDaemonStatus,
  synchronizeRecipeSchedules,
  type RecipeScheduleSyncItem,
} from "./recipeScheduleClient";
import { runRecipeNow } from "./recipeRunner";
import { errorMessage } from "./errors";

const TICK_INTERVAL_MS = 30_000;
const SYNC_DEBOUNCE_MS = 250;
const DAEMON_CHANGED_EVENT = "daemon://changed";

let tickTimer: ReturnType<typeof setInterval> | null = null;
let syncTimer: ReturnType<typeof setTimeout> | null = null;
let lastCheckedAtMs = Date.now();
const inFlight = new Set<string>();
let fallbackReady = false;
let synchronization: Promise<void> | null = null;
let resyncRequested = false;
let unsubscribeAutomations: (() => void) | null = null;
let unsubscribeRecipes: (() => void) | null = null;
let unlistenDaemon: (() => void) | null = null;
let started = false;

function errorText(error: unknown): string {
  return errorMessage(error);
}

export function buildRecipeScheduleSyncItems(
  entries: AutomationEntry[],
  recipes: DiscoveredRecipe[],
): RecipeScheduleSyncItem[] {
  const visible = new Map(
    recipes
      .filter((entry) => entry.recipe !== null && entry.error === null)
      .map((entry) => [entry.recipe!.name, entry] as const),
  );
  return entries.map((entry) => ({
    entryId: entry.id,
    recipeName: entry.recipeName,
    recipePath: visible.get(entry.recipeName)?.path ?? null,
    cron: entry.cron,
    enabled: entry.enabled,
    permissionModeOverride: entry.permissionModeOverride ?? null,
  }));
}

/**
 * Whether an in-app fallback occurrence is due. A daemon delivery timestamp
 * is an additional floor, preventing a just-uninstalled daemon's last run
 * from being repeated by the webview.
 */
export async function isEntryDue(
  entry: AutomationEntry,
  checkedSinceMs: number,
  daemonLastDeliveryAtMs = 0,
): Promise<boolean> {
  try {
    const previous = await invoke<number>("cron_previous", { expr: entry.cron });
    const baseline = entry.catchUpIfMissed
      ? Math.max(entry.lastRunAt ?? 0, daemonLastDeliveryAtMs)
      : Math.max(checkedSinceMs, daemonLastDeliveryAtMs);
    return previous > baseline;
  } catch {
    return false;
  }
}

async function runEntry(entry: AutomationEntry): Promise<void> {
  const recipe = useRecipeStore.getState().recipes
    .find((candidate) => candidate.recipe?.name === entry.recipeName)?.recipe;
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

async function synchronizeOnce(): Promise<void> {
  const automations = useAutomationsStore.getState();
  const recipes = useRecipeStore.getState();

  if (!automations.hydrated || recipes.loading || recipes.error !== null) {
    fallbackReady = false;
    try {
      const status = await recipeSchedulerDaemonStatus();
      automations.setSchedulerRuntime({
        authority: status.installed ? "daemon" : "in_app",
        daemonRunning: status.serviceRunning,
        synchronizedAtMs: null,
        syncError: !automations.hydrated
          ? "Schedules are paused until the saved automation file loads successfully."
          : recipes.loading
            ? "Schedules are paused while recipes are loading."
            : `Schedules were left unchanged because recipes could not be loaded: ${recipes.error}`,
        issues: {},
        lastDeliveryAtMs: automations.scheduler.lastDeliveryAtMs,
      });
    } catch (error) {
      automations.setSchedulerRuntime({
        authority: "unknown",
        daemonRunning: false,
        synchronizedAtMs: null,
        syncError: `Could not determine scheduler authority: ${errorText(error)}`,
        issues: {},
        lastDeliveryAtMs: automations.scheduler.lastDeliveryAtMs,
      });
    }
    return;
  }

  try {
    const result = await synchronizeRecipeSchedules(
      buildRecipeScheduleSyncItems(automations.persistedEntries, recipes.recipes),
    );
    const issues = Object.fromEntries(result.issues.map((issue) => [issue.entryId, issue.message]));
    fallbackReady = result.authority === "in_app";
    automations.setSchedulerRuntime({
      authority: result.authority,
      daemonRunning: result.serviceRunning,
      synchronizedAtMs: result.synchronizedAtMs,
      syncError: null,
      issues,
      lastDeliveryAtMs: result.lastDeliveryAtMs,
    });
  } catch (error) {
    // Never guess that the daemon is absent. An unknown authority pauses the
    // webview fallback and therefore cannot duplicate a persistent trigger.
    fallbackReady = false;
    automations.setSchedulerRuntime({
      authority: "unknown",
      daemonRunning: false,
      synchronizedAtMs: null,
      syncError: `Schedule reconciliation failed: ${errorText(error)}`,
      issues: {},
      lastDeliveryAtMs: automations.scheduler.lastDeliveryAtMs,
    });
  }
}

export function synchronizeSchedulerAuthority(): Promise<void> {
  if (synchronization) {
    resyncRequested = true;
    return synchronization;
  }
  synchronization = (async () => {
    do {
      resyncRequested = false;
      await synchronizeOnce();
    } while (resyncRequested);
  })().finally(() => {
    synchronization = null;
  });
  return synchronization;
}

function requestSynchronization(): void {
  if (syncTimer !== null) clearTimeout(syncTimer);
  syncTimer = setTimeout(() => {
    syncTimer = null;
    void synchronizeSchedulerAuthority();
  }, SYNC_DEBOUNCE_MS);
}

async function tick(): Promise<void> {
  await synchronizeSchedulerAuthority();
  const now = Date.now();
  const automations = useAutomationsStore.getState();
  if (!fallbackReady || automations.scheduler.authority !== "in_app") {
    lastCheckedAtMs = now;
    return;
  }
  if (Object.keys(useSessionStore.getState().runningTurns).length > 0) {
    lastCheckedAtMs = now;
    return;
  }

  const dueChecks = automations.persistedEntries
    .filter((entry) => entry.enabled && !inFlight.has(entry.id));
  for (const entry of dueChecks) {
    const daemonLastDelivery = automations.scheduler.lastDeliveryAtMs[entry.id] ?? 0;
    if (!(await isEntryDue(entry, lastCheckedAtMs, daemonLastDelivery))) continue;
    inFlight.add(entry.id);
    void runEntry(entry).finally(() => inFlight.delete(entry.id));
    break;
  }
  lastCheckedAtMs = now;
}

export function startScheduler(): void {
  if (started) return;
  started = true;
  lastCheckedAtMs = Date.now();
  unsubscribeAutomations = useAutomationsStore.subscribe((state, previous) => {
    if (
      state.persistedEntries !== previous.persistedEntries
      || state.hydrated !== previous.hydrated
    ) {
      requestSynchronization();
    }
  });
  unsubscribeRecipes = useRecipeStore.subscribe((state, previous) => {
    if (
      state.recipes !== previous.recipes
      || state.loading !== previous.loading
      || state.error !== previous.error
    ) {
      requestSynchronization();
    }
  });
  void listen(DAEMON_CHANGED_EVENT, requestSynchronization)
    .then((unlisten) => {
      if (started) unlistenDaemon = unlisten;
      else unlisten();
    })
    .catch((error) => console.error("Failed to subscribe to daemon authority changes", error));
  void synchronizeSchedulerAuthority();
  tickTimer = setInterval(() => void tick(), TICK_INTERVAL_MS);
}

export function stopScheduler(): void {
  if (tickTimer !== null) clearInterval(tickTimer);
  if (syncTimer !== null) clearTimeout(syncTimer);
  tickTimer = null;
  syncTimer = null;
  unsubscribeAutomations?.();
  unsubscribeRecipes?.();
  unlistenDaemon?.();
  unsubscribeAutomations = null;
  unsubscribeRecipes = null;
  unlistenDaemon = null;
  started = false;
  fallbackReady = false;
  resyncRequested = false;
  inFlight.clear();
}

export async function runSchedulerTickForTests(): Promise<void> {
  await tick();
}
