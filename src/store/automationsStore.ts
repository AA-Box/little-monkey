import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { RecipeSchedulerAuthority } from "../lib/recipeScheduleClient";

/** Emitted by the backend after a successful `automations_save` (see
 * src-tauri/src/automations.rs), with the saving window's label as payload —
 * same cross-window sync mechanism as `promptStore.ts`/`sessionStore.ts`. */
const AUTOMATIONS_CHANGED_EVENT = "automations://changed";

/** How long after the last mutation the debounced file write fires — same
 * value/rationale as `sessionStore.ts`'s `PERSIST_DEBOUNCE_MS`. */
const PERSIST_DEBOUNCE_MS = 400;

export type AutomationRunStatus = "ok" | "error" | "denied";

/** One scheduled recipe run — `scheduler.ts`'s 30s tick reads/writes these.
 * The backend never parses this shape (see `automations.rs`'s module doc):
 * it's an opaque JSON blob on that side, exactly like `sessionStore.ts`'s
 * own persistence. */
export interface AutomationEntry {
  id: string;
  /** The recipe this entry runs — resolved by name via `recipeStore.ts`/
   * `recipes_render` at tick time, not snapshotted here, so editing the
   * recipe always affects its next scheduled run. */
  recipeName: string;
  /** A cron expression (croner syntax — see `automations.rs::cron_validate`),
   * e.g. "0 3 * * *" for 3 AM daily. */
  cron: string;
  enabled: boolean;
  /** Overrides the recipe's own `permission_mode` for scheduled runs only —
   * `null`/absent uses the recipe's own mode. */
  permissionModeOverride?: string;
  lastRunAt?: number;
  lastStatus?: AutomationRunStatus;
  lastSessionId?: string;
  /** If a scheduled time was missed while the app was closed, run it once
   * the app is next open instead of silently skipping it. */
  catchUpIfMissed: boolean;
}

interface PersistedShape {
  version: 1;
  entries: AutomationEntry[];
}

export interface SchedulerRuntimeState {
  authority: RecipeSchedulerAuthority | "unknown";
  daemonRunning: boolean;
  synchronizedAtMs: number | null;
  syncError: string | null;
  issues: Record<string, string>;
  lastDeliveryAtMs: Record<string, number>;
}

export interface AutomationsStore {
  entries: AutomationEntry[];
  /** Last snapshot successfully loaded from or committed to Rust. Scheduler
   * authority is reconciled from this durable set, never from optimistic UI
   * edits that may still fail to save. */
  persistedEntries: AutomationEntry[];
  persistError: string | null;
  /** True only after the authoritative automations file was read
   * successfully. The scheduler fails closed while this is false so a
   * transient load error cannot erase daemon triggers. */
  hydrated: boolean;
  scheduler: SchedulerRuntimeState;
  addEntry: (input: Omit<AutomationEntry, "id">) => AutomationEntry;
  updateEntry: (id: string, patch: Partial<Omit<AutomationEntry, "id">>) => void;
  removeEntry: (id: string) => void;
  /** Records the outcome of a scheduler-driven run — called by
   * `scheduler.ts` after every attempt, success or failure. */
  recordRun: (id: string, status: AutomationRunStatus, sessionId?: string) => void;
  setSchedulerRuntime: (runtime: SchedulerRuntimeState) => void;
}

function normalizeEntry(raw: Partial<AutomationEntry>): AutomationEntry | null {
  if (typeof raw.recipeName !== "string" || raw.recipeName.length === 0) return null;
  if (typeof raw.cron !== "string" || raw.cron.length === 0) return null;
  return {
    id: typeof raw.id === "string" && raw.id.length > 0 ? raw.id : crypto.randomUUID(),
    recipeName: raw.recipeName,
    cron: raw.cron,
    enabled: raw.enabled === true,
    permissionModeOverride: typeof raw.permissionModeOverride === "string" ? raw.permissionModeOverride : undefined,
    lastRunAt: typeof raw.lastRunAt === "number" ? raw.lastRunAt : undefined,
    lastStatus: raw.lastStatus === "ok" || raw.lastStatus === "error" || raw.lastStatus === "denied" ? raw.lastStatus : undefined,
    lastSessionId: typeof raw.lastSessionId === "string" ? raw.lastSessionId : undefined,
    catchUpIfMissed: raw.catchUpIfMissed === true,
  };
}

function parsePersisted(raw: string | null): PersistedShape | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as { version?: unknown; entries?: unknown } | null;
    if (!parsed || !Array.isArray(parsed.entries)) return null;
    return {
      version: 1,
      entries: (parsed.entries as unknown[])
        .filter((e): e is Partial<AutomationEntry> => !!e && typeof e === "object")
        .map(normalizeEntry)
        .filter((e): e is AutomationEntry => e !== null),
    };
  } catch {
    return null;
  }
}

let persistTimer: ReturnType<typeof setTimeout> | null = null;
let pendingPayload: string | null = null;
let persistQueue: Promise<void> = Promise.resolve();

function flushPersist(): void {
  if (persistTimer !== null) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  const payload = pendingPayload;
  pendingPayload = null;
  if (payload === null) return;

  const persisted = parsePersisted(payload)?.entries ?? [];
  persistQueue = persistQueue.then(async () => {
    try {
      await invoke("automations_save", { payload });
      useAutomationsStore.setState({ persistedEntries: persisted, persistError: null });
    } catch (err) {
      useAutomationsStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    }
  });
}

/** Flushes the latest optimistic snapshot through the Rust durability
 * boundary and waits until its persisted mirror is settled. The scheduler
 * observes that mirror, so failed writes can never leak into daemon state. */
export async function flushAutomationsPersistence(): Promise<void> {
  flushPersist();
  await persistQueue;
}

function persist(entries: AutomationEntry[]): void {
  if (!isTauri()) return;
  try {
    pendingPayload = JSON.stringify({ version: 1, entries } satisfies PersistedShape);
  } catch (err) {
    useAutomationsStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    return;
  }
  if (persistTimer === null) {
    persistTimer = setTimeout(flushPersist, PERSIST_DEBOUNCE_MS);
  }
}

if (typeof window !== "undefined") {
  window.addEventListener("beforeunload", flushPersist);
}

async function rehydrateFromFile(): Promise<void> {
  let fromFile: PersistedShape | null = null;
  try {
    const raw = await invoke<string | null>("automations_load");
    fromFile = parsePersisted(raw);
    if (raw !== null && !fromFile) {
      useAutomationsStore.setState({
        hydrated: false,
        persistError: "Saved automations are invalid; daemon schedules were left unchanged.",
      });
      return;
    }
  } catch {
    return;
  }
  useAutomationsStore.setState({
    entries: fromFile?.entries ?? [],
    persistedEntries: fromFile?.entries ?? [],
    hydrated: true,
    persistError: null,
  });
}

let subscribed = false;

/** Loads the persisted automations blob and subscribes this window to other
 * windows' saves — called once from `App.tsx`'s boot effect, alongside
 * `subscribeToRecipeChanges`. */
export async function hydrateAutomations(): Promise<void> {
  if (!isTauri()) return;
  if (!subscribed) {
    subscribed = true;
    const ownLabel = getCurrentWindow().label;
    void listen<string>(AUTOMATIONS_CHANGED_EVENT, (event) => {
      if (event.payload === ownLabel) return;
      if (pendingPayload !== null) return;
      void rehydrateFromFile();
    }).catch((err: unknown) => {
      console.error("Failed to subscribe to cross-window automations sync", err);
    });
  }

  try {
    const raw = await invoke<string | null>("automations_load");
    const fromFile = parsePersisted(raw);
    if (raw !== null && !fromFile) {
      useAutomationsStore.setState({
        hydrated: false,
        persistError: "Saved automations are invalid; daemon schedules were left unchanged.",
      });
      return;
    }
    useAutomationsStore.setState({
      entries: fromFile?.entries ?? [],
      persistedEntries: fromFile?.entries ?? [],
      hydrated: true,
      persistError: null,
    });
  } catch (err) {
    useAutomationsStore.setState({
      hydrated: false,
      persistError: err instanceof Error ? err.message : String(err),
    });
  }
}

export const useAutomationsStore = create<AutomationsStore>((set) => ({
  entries: [],
  persistedEntries: [],
  persistError: null,
  hydrated: false,
  scheduler: {
    authority: "unknown",
    daemonRunning: false,
    synchronizedAtMs: null,
    syncError: null,
    issues: {},
    lastDeliveryAtMs: {},
  },

  addEntry: (input) => {
    const entry: AutomationEntry = { id: crypto.randomUUID(), ...input };
    set((state) => {
      const entries = [...state.entries, entry];
      persist(entries);
      return { entries };
    });
    return entry;
  },

  updateEntry: (id, patch) => {
    set((state) => {
      if (!state.entries.some((e) => e.id === id)) return state;
      const entries = state.entries.map((e) => (e.id === id ? { ...e, ...patch } : e));
      persist(entries);
      return { entries };
    });
  },

  removeEntry: (id) => {
    set((state) => {
      if (!state.entries.some((e) => e.id === id)) return state;
      const entries = state.entries.filter((e) => e.id !== id);
      persist(entries);
      return { entries };
    });
  },

  recordRun: (id, status, sessionId) => {
    set((state) => {
      if (!state.entries.some((e) => e.id === id)) return state;
      const entries = state.entries.map((e) =>
        e.id === id ? { ...e, lastRunAt: Date.now(), lastStatus: status, lastSessionId: sessionId ?? e.lastSessionId } : e,
      );
      persist(entries);
      return { entries };
    });
  },

  setSchedulerRuntime: (scheduler) => set({ scheduler }),
}));
