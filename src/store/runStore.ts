import { create } from "zustand";

import {
  checkRunLedgerIntegrity,
  getRun,
  listRuns,
  loadRunEvents,
  onRunsChanged,
  type RunEventEnvelopeWire,
  type RunLedgerIntegrity,
  type RunRecord,
} from "../lib/runProtocol";

interface RunStoreState {
  runs: RunRecord[];
  selectedRunId: string | null;
  eventsByRun: Record<string, RunEventEnvelopeWire[]>;
  loading: boolean;
  detailLoading: boolean;
  error: string | null;
  integrity: RunLedgerIntegrity | null;
  refresh: () => Promise<void>;
  selectRun: (runId: string | null) => Promise<void>;
  refreshRun: (runId: string) => Promise<void>;
  checkIntegrity: () => Promise<void>;
  clearError: () => void;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

let listGeneration = 0;
const detailGenerations = new Map<string, number>();

export const useRunStore = create<RunStoreState>((set, get) => ({
  runs: [],
  selectedRunId: null,
  eventsByRun: {},
  loading: false,
  detailLoading: false,
  error: null,
  integrity: null,

  refresh: async () => {
    const generation = ++listGeneration;
    set({ loading: true, error: null });
    try {
      const runs = await listRuns();
      if (generation !== listGeneration) return;
      const currentSelection = get().selectedRunId;
      const selectedRunId = currentSelection && runs.some((run) => run.spec.run_id === currentSelection)
        ? currentSelection
        : runs[0]?.spec.run_id ?? null;
      set({ runs, selectedRunId, loading: false });
      if (selectedRunId) await get().refreshRun(selectedRunId);
    } catch (error) {
      if (generation === listGeneration) set({ loading: false, error: errorMessage(error) });
    }
  },

  selectRun: async (runId) => {
    set({ selectedRunId: runId, error: null });
    if (runId) await get().refreshRun(runId);
  },

  refreshRun: async (runId) => {
    const generation = (detailGenerations.get(runId) ?? 0) + 1;
    detailGenerations.set(runId, generation);
    set({ detailLoading: true });
    try {
      const [run, events] = await Promise.all([getRun(runId), loadRunEvents(runId)]);
      if (detailGenerations.get(runId) !== generation) return;
      set((state) => ({
        runs: run
          ? [run, ...state.runs.filter((entry) => entry.spec.run_id !== runId)].sort(
              (a, b) => b.spec.created_at_ms - a.spec.created_at_ms || b.spec.run_id.localeCompare(a.spec.run_id),
            )
          : state.runs.filter((entry) => entry.spec.run_id !== runId),
        eventsByRun: { ...state.eventsByRun, [runId]: events },
        detailLoading: false,
      }));
    } catch (error) {
      if (detailGenerations.get(runId) === generation) {
        set({ detailLoading: false, error: errorMessage(error) });
      }
    }
  },

  checkIntegrity: async () => {
    set({ error: null });
    try {
      set({ integrity: await checkRunLedgerIntegrity() });
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  clearError: () => set({ error: null }),
}));

let unlisten: (() => void) | null = null;
let subscriptionPromise: Promise<void> | null = null;

/** Installs exactly one cross-window ledger listener and loads initial state.
 * Safe to call repeatedly from React StrictMode/HMR. */
export function initializeRunStore(): Promise<void> {
  if (subscriptionPromise) return subscriptionPromise;
  subscriptionPromise = (async () => {
    unlisten = await onRunsChanged((payload) => {
      void useRunStore.getState().refreshRun(payload.runId);
    });
    await useRunStore.getState().refresh();
  })().catch((error) => {
    subscriptionPromise = null;
    useRunStore.setState({ error: errorMessage(error), loading: false });
  });
  return subscriptionPromise;
}

export function disposeRunStoreSubscription(): void {
  unlisten?.();
  unlisten = null;
  subscriptionPromise = null;
}
