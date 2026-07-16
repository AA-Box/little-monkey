import { create } from "zustand";

import * as api from "../lib/issueToPr";
import type { IssueToPrRun } from "../lib/issueToPr";
import { isTerminalIssueToPrStatus } from "../lib/issueToPr";
import { runIssueToPrAgent } from "../lib/issueToPrRunner";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** In-flight cancellation handles, keyed by run id — deliberately NOT part of
 * the zustand state (an `AbortController` isn't a value React/zustand needs
 * to react to, and storing it in state would make the store non-serializable
 * for no benefit). */
const controllers = new Map<string, AbortController>();
let unlisten: (() => void) | null = null;

interface IssueToPrState {
  runs: IssueToPrRun[];
  selectedRunId: string | null;
  activityByRun: Record<string, string>;
  busy: Record<string, boolean>;
  error: string | null;
  notice: string | null;

  init: () => Promise<void>;
  refresh: () => Promise<void>;
  selectRun: (runId: string | null) => void;
  start: (issueUrl: string) => Promise<IssueToPrRun>;
  cancel: (runId: string) => Promise<void>;
  markPrOpened: (runId: string, prNumber: number, prUrl: string) => Promise<void>;
  markDone: (runId: string) => Promise<void>;
  clearMessages: () => void;
}

function upsertRun(runs: IssueToPrRun[], run: IssueToPrRun): IssueToPrRun[] {
  const index = runs.findIndex((candidate) => candidate.runId === run.runId);
  if (index === -1) return [run, ...runs];
  const next = [...runs];
  next[index] = run;
  return next;
}

export const useIssueToPrStore = create<IssueToPrState>((set, get) => {
  const perform = async <T>(key: string, task: () => Promise<T>): Promise<T> => {
    set((state) => ({ busy: { ...state.busy, [key]: true }, error: null }));
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set((state) => ({ busy: { ...state.busy, [key]: false } }));
    }
  };

  const onProgress = (run: IssueToPrRun) => {
    set((state) => ({ runs: upsertRun(state.runs, run) }));
    if (isTerminalIssueToPrStatus(run.status)) {
      controllers.delete(run.runId);
      set((state) => {
        const activityByRun = { ...state.activityByRun };
        delete activityByRun[run.runId];
        return { activityByRun };
      });
    }
  };

  /** Drives a started run from "planning" through the headless agent turn,
   * the target repository's own checks, and up to "opening_pr" (or
   * "failed") — everything after that (pushing the branch and opening the
   * draft PR) is a real external GitHub write and stays a human-confirmed
   * step in the panel, reusing `gitDelivery.ts`'s existing confirm-and-type-
   * the-phrase flow rather than anything in this function. Never throws —
   * every failure path reports itself onto the run record via
   * `advanceIssueToPr` instead. */
  const driveRun = async (run: IssueToPrRun): Promise<void> => {
    const controller = new AbortController();
    controllers.set(run.runId, controller);
    try {
      await api.advanceIssueToPr(run.runId, "implementing");
      const result = await runIssueToPrAgent({
        runId: run.runId,
        repositorySlug: run.repositorySlug,
        issueNumber: run.issueNumber,
        issueTitle: run.issueTitle,
        issueBody: run.issueBody,
        branch: run.branch,
        workspaceLabel: run.workspaceLabel,
        signal: controller.signal,
        onToolActivity: (label) => {
          set((state) => ({ activityByRun: { ...state.activityByRun, [run.runId]: label } }));
        },
      });

      if (result.outcome === "cancelled") {
        // The Cancel action already calls `issue_to_pr_cancel` itself
        // (see `cancel` below) — nothing left to record here.
        return;
      }
      if (result.outcome === "error") {
        await api.advanceIssueToPr(run.runId, "failed", {
          error: result.summary,
          durableRunId: result.durableRunId,
        });
        return;
      }
      if (result.durableRunId) {
        await api
          .advanceIssueToPr(run.runId, run.status, { durableRunId: result.durableRunId })
          .catch(() => {});
      }
      await api.runIssueToPrChecks(run.runId);
    } catch (error) {
      await api.advanceIssueToPr(run.runId, "failed", { error: errorText(error) }).catch(() => {});
    } finally {
      controllers.delete(run.runId);
    }
  };

  return {
    runs: [],
    selectedRunId: null,
    activityByRun: {},
    busy: {},
    error: null,
    notice: null,

    clearMessages: () => set({ error: null, notice: null }),

    init: async () => {
      if (!unlisten) {
        unlisten = await api.listenIssueToPrProgress(onProgress);
      }
      await get().refresh();
    },

    refresh: () =>
      perform("refresh", async () => {
        const runs = await api.listIssueToPrRuns();
        set((state) => ({
          runs,
          selectedRunId: state.selectedRunId && runs.some((run) => run.runId === state.selectedRunId)
            ? state.selectedRunId
            : runs[0]?.runId ?? null,
        }));
      }),

    selectRun: (runId) => set({ selectedRunId: runId }),

    start: (issueUrl) =>
      perform("start", async () => {
        const run = await api.startIssueToPr(issueUrl);
        set((state) => ({ runs: upsertRun(state.runs, run), selectedRunId: run.runId }));
        void driveRun(run);
        return run;
      }),

    cancel: (runId) =>
      perform(`cancel:${runId}`, async () => {
        controllers.get(runId)?.abort();
        const run = await api.cancelIssueToPr(runId);
        set((state) => ({ runs: upsertRun(state.runs, run) }));
      }),

    markPrOpened: (runId, prNumber, prUrl) =>
      perform(`pr:${runId}`, async () => {
        const run = await api.advanceIssueToPr(runId, "awaiting_review", { prNumber, prUrl });
        set((state) => ({ runs: upsertRun(state.runs, run) }));
      }),

    markDone: (runId) =>
      perform(`done:${runId}`, async () => {
        const run = await api.advanceIssueToPr(runId, "done");
        set((state) => ({ runs: upsertRun(state.runs, run) }));
      }),
  };
});
