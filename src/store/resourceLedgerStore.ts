import { create } from "zustand";

import { errorMessage } from "../lib/errors";
import { daemonDecisions, type SchedulerDecision } from "../lib/daemonClient";
import {
  processUsageLedger,
  type ProcessEgressDestinations,
  type ProcessUsageAggregate,
  type ProcessUsageRow,
} from "../lib/processUsage";

/**
 * Read-only mirror of the two K6/K8 inspection surfaces: the per-process
 * resource ledger (`process_usage_ledger`) and the scheduler decision log
 * (`monkey daemon decisions`).
 *
 * Both are read on demand rather than polled. Neither is a live dashboard — a
 * cost ledger is read when somebody asks what a turn cost, and a decision log
 * when somebody asks why a job waited. The CLI comment on `daemon decisions`
 * says the same thing, and it is why that log is its own command instead of
 * more fields on the status payload the desktop polls every two seconds.
 *
 * The two reads keep separate `error` slots on purpose: the decision log needs a
 * Tauri command that does not exist yet, so its failure is expected today and
 * must not blank out a resource ledger that read fine.
 */

/** Bounded like every other listing here — the ledger holds exited rows forever. */
const ROW_LIMIT = 200;

/** Well under the daemon's own 512-row cap; a log read by a human scrolls. */
const DECISION_LIMIT = 100;

interface ResourceLedgerStore {
  rows: ProcessUsageRow[];
  totals: ProcessUsageAggregate | null;
  /** Where each row's allowed egress went, keyed by `processId`. Read through
   * `destinationsFor`, which treats a missing key and an empty record alike. */
  destinations: Record<string, ProcessEgressDestinations>;
  /** Whether the ledger read was scoped to exited processes. A live process has
   * no closed-out row, so its measurements are mostly unavailable-with-reason —
   * true information, but it buries the rows that have numbers. */
  closedOnly: boolean;
  loadingLedger: boolean;
  ledgerError: string | null;

  decisions: SchedulerDecision[];
  loadingDecisions: boolean;
  decisionsError: string | null;

  refreshLedger: () => Promise<void>;
  setClosedOnly: (closedOnly: boolean) => Promise<void>;
  refreshDecisions: () => Promise<void>;
}

export const useResourceLedgerStore = create<ResourceLedgerStore>((set, get) => ({
  rows: [],
  totals: null,
  destinations: {},
  closedOnly: true,
  loadingLedger: false,
  ledgerError: null,

  decisions: [],
  loadingDecisions: false,
  decisionsError: null,

  refreshLedger: async () => {
    set({ loadingLedger: true, ledgerError: null });
    try {
      const ledger = await processUsageLedger({ closedOnly: get().closedOnly, limit: ROW_LIMIT });
      // `null` outside Tauri (dev/browser profile): no backend, so no rows —
      // not an error, and not a reason to clear a previous read either.
      // `destinations` defaults rather than being read blind: a mocked or
      // older backend that omits it must leave the map empty, not undefined.
      if (ledger) set({ rows: ledger.rows, totals: ledger.totals, destinations: ledger.destinations ?? {} });
      set({ loadingLedger: false });
    } catch (error) {
      set({ loadingLedger: false, ledgerError: errorMessage(error) });
    }
  },

  setClosedOnly: async (closedOnly) => {
    set({ closedOnly });
    await get().refreshLedger();
  },

  refreshDecisions: async () => {
    set({ loadingDecisions: true, decisionsError: null });
    try {
      set({ decisions: await daemonDecisions(DECISION_LIMIT), loadingDecisions: false });
    } catch (error) {
      // Surfaced, not swallowed: today this is the missing Tauri command, and a
      // panel that silently shows an empty decision log would read as "the
      // scheduler made no decisions", which is a different and false claim.
      set({ loadingDecisions: false, decisionsError: errorMessage(error) });
    }
  },
}));
