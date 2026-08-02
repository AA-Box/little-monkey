import { create } from "zustand";

import { errorMessage } from "../lib/errors";
import {
  listProcesses,
  onProcessesChanged,
  processDisplayState,
  signalProcess,
  type ProcessDisplayState,
  type ProcessRecord,
  type ProcessSignal,
} from "../lib/processTable";

/**
 * A read-and-signal mirror of the unified agent process table
 * (`process_table.rs`), for the Processes panel.
 *
 * Rust owns the records; this store never invents one. It reads the current
 * listing once and then follows `processes://changed`, the same convention
 * `backgroundShellStore` uses for its own events — so a turn started in
 * another window, a daemon job, or a `monkey processes signal` from a terminal
 * all show up here without polling.
 *
 * Scoped to live processes by default. An exited row is history, and the
 * ledger keeps it; this surface is about what is running right now and what
 * can be signalled.
 */

/** How many live records the panel will hold. Far above any real live count —
 * a bound against a runaway producer, not a paging feature. */
const LIVE_LIMIT = 500;

interface ProcessStore {
  records: ProcessRecord[];
  loading: boolean;
  /** Last read/signal failure, shown inline rather than thrown away. A refused
   * signal carries the refusal reason from `ProcessKind::signal_support`,
   * which is the whole point of refusals being typed. */
  error: string | null;
  /** Process ids with a signal in flight, so the row can disable its own
   * buttons without a component-local flag per row. */
  pending: Record<string, true>;
  refresh: () => Promise<void>;
  /** Applies one record from `processes://changed`. Exported on the store (not
   * just wired internally) so a test can drive the event path directly. */
  applyRecord: (record: ProcessRecord) => void;
  signal: (processId: string, signal: ProcessSignal, reason?: string) => Promise<void>;
  clearError: () => void;
}

function sortRecords(records: ProcessRecord[]): ProcessRecord[] {
  // Newest first, matching `process_list`'s own ordering, so a record arriving
  // by event lands where a refresh would have put it.
  return [...records].sort((a, b) => b.createdAtMs - a.createdAtMs);
}

export const useProcessStore = create<ProcessStore>((set, get) => ({
  records: [],
  loading: false,
  error: null,
  pending: {},

  refresh: async () => {
    set({ loading: true });
    try {
      const records = await listProcesses({ liveOnly: true, limit: LIVE_LIMIT });
      set({ records: sortRecords(records), loading: false });
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
    }
  },

  applyRecord: (record) => {
    set((state) => {
      const rest = state.records.filter((entry) => entry.processId !== record.processId);
      // An exited process leaves the live listing rather than lingering as a
      // greyed-out row — `refresh` would not have returned it either.
      if (record.state === "exited") return { records: rest };
      return { records: sortRecords([...rest, record]) };
    });
  },

  signal: async (processId, signal, reason) => {
    set((state) => ({ pending: { ...state.pending, [processId]: true }, error: null }));
    try {
      const record = await signalProcess(processId, signal, reason);
      get().applyRecord(record);
    } catch (error) {
      // Deliberately surfaced: a refusal names the kind's own reason, and a
      // silently-ignored button is worse than a visible "this kind can't".
      set({ error: errorMessage(error) });
    } finally {
      set((state) => {
        const pending = { ...state.pending };
        delete pending[processId];
        return { pending };
      });
    }
  },

  clearError: () => set({ error: null }),
}));

/**
 * Subscribes the store to `processes://changed`. Returns the unsubscribe.
 *
 * Called by the panel rather than at module load, so a window that never opens
 * the panel carries no listener.
 */
export async function subscribeToProcessChanges(): Promise<() => void> {
  return onProcessesChanged((record) => useProcessStore.getState().applyRecord(record));
}

export interface ProcessGroupCount {
  state: ProcessDisplayState;
  count: number;
}

/**
 * Live counts by displayed state — the panel's header summary. Derived, so a
 * `pause_pending` row is counted as such rather than as one more `running`.
 *
 * Takes the records rather than the store because it builds a fresh array of
 * fresh objects every call: subscribing to it as a selector (even through
 * `useShallow`, which compares an array's elements by identity) re-renders on
 * every store read and trips React's update-depth guard. Callers pass the
 * already-subscribed `records` and memoize on it.
 */
export function selectStateCounts(records: readonly ProcessRecord[]): ProcessGroupCount[] {
  const counts = new Map<ProcessDisplayState, number>();
  for (const record of records) {
    const display = processDisplayState(record);
    counts.set(display, (counts.get(display) ?? 0) + 1);
  }
  return [...counts.entries()].map(([displayState, count]) => ({ state: displayState, count }));
}

export function selectLiveProcessCount(state: ProcessStore): number {
  return state.records.length;
}
