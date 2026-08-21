import { create } from "zustand";

import * as api from "../lib/issueToPr";
import type { IssueToPrRun } from "../lib/issueToPr";
import { isTerminalIssueToPrStatus } from "../lib/issueToPr";
import * as issueToPrRunner from "../lib/issueToPrRunner";
import { errorMessage } from "../lib/errors";

function errorText(error: unknown): string {
  return errorMessage(error);
}

/** In-flight cancellation handles, keyed by run id — deliberately NOT part of
 * the zustand state (an `AbortController` isn't a value React/zustand needs
 * to react to, and storing it in state would make the store non-serializable
 * for no benefit). */
const controllers = new Map<string, AbortController>();
let unlisten: (() => void) | null = null;
const runIssueToPrAgent = issueToPrRunner.runIssueToPrAgent;
const autonomousIssueRunner = "runIssueToPrAutonomousTask" in issueToPrRunner
  ? issueToPrRunner.runIssueToPrAutonomousTask
  : null;

/** Test-only: clears in-flight run controllers. This module's `controllers`
 * map is process-lifetime by design (see the comment above it), which means
 * it otherwise leaks across every test in the same file. */
export function __resetIssueToPrControllersForTests(): void {
  controllers.clear();
}

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
      const result = await (autonomousIssueRunner ?? runIssueToPrAgent)({
        runId: run.runId,
        repositorySlug: run.repositorySlug,
        issueNumber: run.issueNumber,
        issueTitle: run.issueTitle,
        issueBody: run.issueBody,
        branch: run.branch,
        worktreeId: run.worktreeId,
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
      const coordinatorOutcome = "task" in result ? (result as { task?: { outcome?: string } }).task?.outcome : undefined;
      if (coordinatorOutcome === "WAITING_USER" || coordinatorOutcome === "WAITING_APPROVAL") {
        await api.advanceIssueToPr(run.runId, "opening_pr");
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
        // Self-report onto "implementing" (the status this run was moved to
        // a few lines up) rather than the closed-over `run.status`, which is
        // still this run's value from before `start()` kicked off the drive
        // and would make the Rust state machine reject the whole call.
        await api
          .advanceIssueToPr(run.runId, "implementing", { durableRunId: result.durableRunId })
          .catch(() => {});
      }
      await api.runIssueToPrChecks(run.runId);
    } catch (error) {
      await api.advanceIssueToPr(run.runId, "failed", { error: errorText(error) }).catch(() => {});
    } finally {
      controllers.delete(run.runId);
    }
  };

  /** Picks a non-terminal run back up after an app restart (or a fresh
   * `init()` in a process that never drove it) — without this, a run left
   * in "planning"/"implementing"/"checking"/"opening_pr" when the app closed
   * has no driver in the new process's `controllers` map and sits stuck
   * forever with Cancel as the only (also inert) action. Takes the
   * `controllers` slot synchronously before any `await` so a second `init()`
   * call racing this one can never double-drive the same run. */
  const resumeOrphanedRun = (run: IssueToPrRun): void => {
    if (controllers.has(run.runId) || isTerminalIssueToPrStatus(run.status)) return;
    if (run.status === "opening_pr" || run.status === "awaiting_review") {
      // Waiting on a human action (push+open-PR, or mark-done) in the panel
      // — nothing to re-drive.
      return;
    }
    if (run.status === "checking") {
      const controller = new AbortController();
      controllers.set(run.runId, controller);
      void (async () => {
        try {
          // Reconfirm the authoritative status before re-triggering a real
          // mutation — the list snapshot this loop iterated over can be
          // stale by the time this async resume actually runs.
          const latest = await api.getIssueToPrStatus(run.runId).catch(() => run);
          if (latest.status === "checking") await api.runIssueToPrChecks(run.runId);
        } catch (error) {
          await api.advanceIssueToPr(run.runId, "failed", { error: errorText(error) }).catch(() => {});
        } finally {
          controllers.delete(run.runId);
        }
      })();
      return;
    }
    // "planning" or "implementing": re-run the headless agent turn against
    // the same owned worktree. Any files it already wrote are still on disk,
    // so the agent picks up from there instead of losing progress; the self-
    // transition `implementing` -> `implementing` this triggers is a legal
    // no-op on the Rust side (see `issue_to_pr.rs`'s `valid_transition`).
    void driveRun(run);
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
      for (const run of get().runs) {
        resumeOrphanedRun(run);
      }
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
