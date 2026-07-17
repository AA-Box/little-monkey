import { create } from "zustand";

import {
  fallbackHeuristicPlan,
  generateMigrationPlan,
  type MigrationPlan,
} from "../lib/migrationAgent";
import { runMigrationSliceAgent, type MigrationSliceAgentResult } from "../lib/migrationAgentRunner";

const STORAGE_KEY = "little-monkey-migration-agent-runs-v1";
const STORAGE_VERSION = 1;

export type MigrationRunStatus =
  | "drafting"
  | "planned"
  | "implementing"
  | "awaiting_push"
  | "completed"
  | "failed"
  | "cancelled";

export const TERMINAL_MIGRATION_RUN_STATUSES: ReadonlySet<MigrationRunStatus> = new Set([
  "completed",
  "failed",
  "cancelled",
]);

export function isTerminalMigrationRunStatus(status: MigrationRunStatus): boolean {
  return TERMINAL_MIGRATION_RUN_STATUSES.has(status);
}

export interface MigrationSliceOutcome {
  sliceId: string;
  outcome: MigrationSliceAgentResult["outcome"];
  summary: string;
  durableRunId: string | null;
  updatedAtMs: number;
}

export interface MigrationRun {
  runId: string;
  goal: string;
  /** Manually entered by the user, same "owner/repository" free-text field
   * `GitDeliveryPanel.tsx` already uses — never auto-detected. */
  repositorySlug: string;
  status: MigrationRunStatus;
  plan: MigrationPlan | null;
  /** Set once the panel has driven `gitDelivery.ts`'s own confirm-and-type-
   * the-phrase flow to create the owned worktree — this store never creates
   * or pushes a worktree/branch itself, exactly like `issueToPrStore.ts`
   * never drives its own push/PR mutation. */
  worktreeId: string | null;
  branch: string | null;
  workspaceLabel: string | null;
  sliceOutcome: MigrationSliceOutcome | null;
  prNumber: number | null;
  prUrl: string | null;
  error: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function persist(runs: MigrationRun[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: STORAGE_VERSION, runs }));
  } catch {
    // The run stays live in memory for this session even if persistence
    // fails (e.g. storage quota) — nothing downstream depends on the write
    // having succeeded.
  }
}

function isMigrationRunStatus(value: unknown): value is MigrationRunStatus {
  return (
    value === "drafting" ||
    value === "planned" ||
    value === "implementing" ||
    value === "awaiting_push" ||
    value === "completed" ||
    value === "failed" ||
    value === "cancelled"
  );
}

function hydrate(): MigrationRun[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as
      | { version?: unknown; runs?: unknown }
      | null;
    if (raw?.version !== STORAGE_VERSION || !Array.isArray(raw.runs)) return [];
    return raw.runs.filter((value): value is MigrationRun => {
      const item = value as Partial<MigrationRun>;
      return Boolean(
        item &&
        typeof item.runId === "string" &&
        typeof item.goal === "string" &&
        typeof item.repositorySlug === "string" &&
        isMigrationRunStatus(item.status) &&
        typeof item.createdAtMs === "number" &&
        typeof item.updatedAtMs === "number",
      );
    });
  } catch {
    return [];
  }
}

/** In-flight cancellation handles, keyed by run id — deliberately NOT part
 * of the zustand state (an `AbortController` isn't a value React/zustand
 * needs to react to), mirroring `issueToPrStore.ts`'s own `controllers` map. */
const controllers = new Map<string, AbortController>();

/** Test-only: clears in-flight run controllers, same purpose as
 * `issueToPrStore.ts`'s `__resetIssueToPrControllersForTests`. */
export function __resetMigrationAgentControllersForTests(): void {
  controllers.clear();
}

interface MigrationAgentState {
  runs: MigrationRun[];
  selectedRunId: string | null;
  activityByRun: Record<string, string>;
  busy: Record<string, boolean>;
  error: string | null;
  notice: string | null;

  init: () => void;
  selectRun: (runId: string | null) => void;
  clearMessages: () => void;
  createRun: (goal: string, repositorySlug: string) => Promise<MigrationRun>;
  regeneratePlan: (runId: string) => Promise<void>;
  attachWorktree: (runId: string, worktreeId: string, branch: string, workspaceLabel: string) => void;
  attemptFirstSlice: (runId: string) => Promise<void>;
  cancel: (runId: string) => void;
  markPrOpened: (runId: string, prNumber: number, prUrl: string) => void;
  markDone: (runId: string) => void;
  deleteRun: (runId: string) => void;
}

function upsertRun(runs: MigrationRun[], run: MigrationRun): MigrationRun[] {
  const index = runs.findIndex((candidate) => candidate.runId === run.runId);
  if (index === -1) return [run, ...runs];
  const next = [...runs];
  next[index] = run;
  return next;
}

function touch(run: MigrationRun, patch: Partial<MigrationRun>): MigrationRun {
  return { ...run, ...patch, updatedAtMs: Date.now() };
}

export const useMigrationAgentStore = create<MigrationAgentState>((set, get) => {
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

  const updateRun = (runId: string, patch: Partial<MigrationRun>): void => {
    set((state) => {
      const existing = state.runs.find((run) => run.runId === runId);
      if (!existing) return state;
      const runs = upsertRun(state.runs, touch(existing, patch));
      persist(runs);
      return { runs };
    });
  };

  return {
    runs: hydrate(),
    selectedRunId: null,
    activityByRun: {},
    busy: {},
    error: null,
    notice: null,

    clearMessages: () => set({ error: null, notice: null }),

    init: () => {
      const runs = hydrate();
      set((state) => ({
        runs,
        selectedRunId: state.selectedRunId && runs.some((run) => run.runId === state.selectedRunId)
          ? state.selectedRunId
          : runs[0]?.runId ?? null,
      }));
    },

    selectRun: (runId) => set({ selectedRunId: runId }),

    createRun: (goal, repositorySlug) =>
      perform("createRun", async () => {
        const trimmedGoal = goal.trim();
        const run: MigrationRun = {
          runId: crypto.randomUUID(),
          goal: trimmedGoal,
          repositorySlug: repositorySlug.trim(),
          status: "drafting",
          plan: null,
          worktreeId: null,
          branch: null,
          workspaceLabel: null,
          sliceOutcome: null,
          prNumber: null,
          prUrl: null,
          error: null,
          createdAtMs: Date.now(),
          updatedAtMs: Date.now(),
        };
        set((state) => {
          const runs = upsertRun(state.runs, run);
          persist(runs);
          return { runs, selectedRunId: run.runId };
        });

        try {
          const plan = await generateMigrationPlan(trimmedGoal);
          updateRun(run.runId, { status: "planned", plan });
        } catch (error) {
          // A model failure at plan time still leaves the run usable (the
          // user can retry via `regeneratePlan`) rather than vanishing —
          // fall back to the generic heuristic plan so the run always has
          // *something* actionable, and surface the real error alongside it.
          updateRun(run.runId, { status: "planned", plan: fallbackHeuristicPlan(trimmedGoal), error: errorText(error) });
        }
        return get().runs.find((candidate) => candidate.runId === run.runId) ?? run;
      }),

    regeneratePlan: (runId) =>
      perform(`plan:${runId}`, async () => {
        const run = get().runs.find((candidate) => candidate.runId === runId);
        if (!run) throw new Error("Unknown migration run.");
        updateRun(runId, { status: "drafting", error: null });
        try {
          const plan = await generateMigrationPlan(run.goal);
          updateRun(runId, { status: "planned", plan });
        } catch (error) {
          updateRun(runId, { status: "planned", error: errorText(error) });
          throw error;
        }
      }),

    attachWorktree: (runId, worktreeId, branch, workspaceLabel) =>
      updateRun(runId, { worktreeId, branch, workspaceLabel }),

    attemptFirstSlice: (runId) =>
      perform(`attempt:${runId}`, async () => {
        const run = get().runs.find((candidate) => candidate.runId === runId);
        if (!run) throw new Error("Unknown migration run.");
        if (!run.plan || run.plan.slices.length === 0) throw new Error("Generate a plan before attempting a slice.");
        if (!run.worktreeId || !run.branch || !run.workspaceLabel) {
          throw new Error("Create the owned worktree for this run before attempting a slice.");
        }
        const slice = run.plan.slices[0];

        const controller = new AbortController();
        controllers.set(runId, controller);
        updateRun(runId, { status: "implementing", error: null });

        try {
          const result = await runMigrationSliceAgent({
            runId,
            goal: run.goal,
            slice,
            branch: run.branch,
            workspaceLabel: run.workspaceLabel,
            signal: controller.signal,
            onToolActivity: (label) => {
              set((state) => ({ activityByRun: { ...state.activityByRun, [runId]: label } }));
            },
          });

          const sliceOutcome: MigrationSliceOutcome = {
            sliceId: slice.id,
            outcome: result.outcome,
            summary: result.summary,
            durableRunId: result.durableRunId,
            updatedAtMs: Date.now(),
          };

          if (result.outcome === "cancelled") {
            updateRun(runId, { status: "cancelled", sliceOutcome });
          } else if (result.outcome === "error") {
            updateRun(runId, { status: "failed", sliceOutcome, error: result.summary });
          } else {
            updateRun(runId, { status: "awaiting_push", sliceOutcome });
          }
        } catch (error) {
          updateRun(runId, { status: "failed", error: errorText(error) });
        } finally {
          controllers.delete(runId);
          set((state) => {
            const activityByRun = { ...state.activityByRun };
            delete activityByRun[runId];
            return { activityByRun };
          });
        }
      }),

    cancel: (runId) => {
      controllers.get(runId)?.abort();
    },

    markPrOpened: (runId, prNumber, prUrl) =>
      updateRun(runId, { status: "completed", prNumber, prUrl }),

    markDone: (runId) => updateRun(runId, { status: "completed" }),

    deleteRun: (runId) => {
      controllers.get(runId)?.abort();
      controllers.delete(runId);
      set((state) => {
        const runs = state.runs.filter((run) => run.runId !== runId);
        persist(runs);
        return {
          runs,
          selectedRunId: state.selectedRunId === runId ? runs[0]?.runId ?? null : state.selectedRunId,
        };
      });
    },
  };
});
