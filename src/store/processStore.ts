import { create } from "zustand";

import { errorMessage } from "../lib/errors";
import { listProcesses, onProcessesChanged, type ProcessRecord } from "../lib/processTable";
import {
  processDisplayState,
  signalProcess,
  type ProcessDisplayState,
  type ProcessSignal,
} from "../lib/processSignals";

/**
 * A read-and-signal mirror of the unified agent process table
 * (`process_table.rs`), for the Processes panel.
 *
 * Rust owns the records; this store never invents one. It reads the current
 * listing once and then follows `processes://changed`, the same convention
 * `backgroundShellStore` uses for its own events — so a turn started in
 * another window or a daemon job shows up here on its own.
 *
 * That event is not enough on its own. `monkey processes signal` writes the
 * SQLite ledger from a *different OS process* and cannot emit a Tauri event
 * into this one, so a row suspended or resumed from a terminal would keep
 * rendering its previous state until something remounted the panel. `catchUp`
 * closes that gap: while the panel is open it re-reads the ledger on a timer,
 * which is the only way one process learns about another's writes here.
 *
 * Scoped to live processes by default. An exited row is history, and the
 * ledger keeps it; this surface is about what is running right now and what
 * can be signalled.
 */

/** How many live records the panel will hold. Far above any real live count —
 * a bound against a runaway producer, not a paging feature. */
const LIVE_LIMIT = 500;

/**
 * How often an open panel re-reads the ledger.
 *
 * Matched to the backend's own `process_pending_signals` sweep: a signal sent
 * from the CLI takes up to one sweep to be delivered, so reading faster than
 * the sweep would only render the latch sooner than the loop can act on it.
 * Only runs while the panel is mounted.
 */
export const PROCESS_CATCH_UP_INTERVAL_MS = 2_000;

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
  /** A quiet re-read of the ledger, for the open panel's timer.
   *
   * Differs from `refresh` in three ways, each of them the point: it never
   * toggles `loading` (a spinner blinking every two seconds reads as breakage),
   * it leaves `records` untouched when the listing is unchanged (so a poll that
   * finds no news costs no re-render anywhere), and it swallows read failures
   * rather than replacing a working panel with a banner that reappears on every
   * tick — mount and the refresh button are where a read failure is surfaced. */
  catchUp: () => Promise<void>;
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

/**
 * Whether two sorted listings would render identically.
 *
 * Every displayed field is compared, not just `updatedAtMs`: a signal writes
 * `updated_at_ms = signal_requested_at_ms` (`process_table.rs`), so two signals
 * landing in the same millisecond would carry the same stamp while differing in
 * what they latched. Comparing the fields the row actually draws makes the
 * "nothing changed" claim true by construction rather than by trusting a clock.
 */
function sameLiveListing(current: readonly ProcessRecord[], next: readonly ProcessRecord[]): boolean {
  if (current.length !== next.length) return false;
  return current.every((entry, index) => {
    const other = next[index];
    return (
      entry.processId === other.processId &&
      entry.state === other.state &&
      entry.updatedAtMs === other.updatedAtMs &&
      entry.nativePid === other.nativePid &&
      entry.signalReason === other.signalReason &&
      entry.signalIntent.stopRequested === other.signalIntent.stopRequested &&
      entry.signalIntent.suspendRequested === other.signalIntent.suspendRequested &&
      entry.signalIntent.killRequested === other.signalIntent.killRequested
    );
  });
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

  catchUp: async () => {
    // A signal in flight already owns the row: `signal` applies the record the
    // command returns, and a listing read before that write landed would flick
    // the row back to its old state for one tick.
    if (Object.keys(get().pending).length > 0) return;
    try {
      const fetched = sortRecords(await listProcesses({ liveOnly: true, limit: LIVE_LIMIT }));
      // Returning the state object itself is zustand's documented no-op: it
      // compares with `Object.is` and skips notifying listeners entirely.
      set((state) => (sameLiveListing(state.records, fetched) ? state : { records: fetched }));
    } catch {
      // Deliberately silent — see the `catchUp` contract above.
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

/**
 * Runs the open panel's catch-up poll. Returns the stop function.
 *
 * Extracted from the component rather than inlined in its effect because the
 * repo's component tests render with `renderToStaticMarkup`, which never runs
 * effects — wiring the timer here is what makes the CLI-visibility fix
 * testable at all.
 *
 * `tick` is called first and separately from the ledger read: the row's age is
 * computed from `Date.now()` at render, so without a repaint it freezes at
 * whatever it read when the panel mounted, whether or not any record changed.
 */
export function startProcessCatchUp(tick: () => void): () => void {
  const timer = setInterval(() => {
    tick();
    void useProcessStore.getState().catchUp();
  }, PROCESS_CATCH_UP_INTERVAL_MS);
  return () => clearInterval(timer);
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
