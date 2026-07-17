import { create } from "zustand";

import type { ResearchPlan, ResearchReport, StepOutcome } from "../lib/deepResearch";

/**
 * Deep Research Workspace (ROADMAP.md Phase 7): the store of record for every
 * research run this app has driven — its plan, its per-step evidence (as
 * each step actually completes, for live progress), its final cited report,
 * and any terminal error. Purely a state container: all the actual
 * model/tool-execution work happens in `../lib/deepResearch.ts`'s
 * `startDeepResearch`/`runDeepResearch`, which call the setters below as
 * each phase completes — mirrors `sideTaskStore.ts`'s split from
 * `sideTaskRunner.ts` (state here, orchestration there).
 *
 * Deliberately NOT persisted: a research run is transient, in-session work,
 * same posture `sideTaskStore.ts` takes for the same reason (closing the app
 * mid-run is the same as cancelling it; nothing here needs to survive a
 * restart).
 */

export type DeepResearchStatus = "planning" | "researching" | "synthesizing" | "done" | "error" | "cancelled";

export interface DeepResearchRun {
  id: string;
  question: string;
  status: DeepResearchStatus;
  plan: ResearchPlan | null;
  /** Completed steps only, in execution order — a step still in flight is
   * instead reflected by `pendingStepId` below, so the panel can show a
   * spinner on it without a half-filled `StepOutcome` entry. */
  stepResults: StepOutcome[];
  /** The plan step currently executing, or null — cleared the moment that
   * step's `StepOutcome` is appended to `stepResults`. */
  pendingStepId: string | null;
  report: ResearchReport | null;
  error: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

interface DeepResearchStoreState {
  runs: Record<string, DeepResearchRun>;
  /** Insertion order, newest first. */
  order: string[];
  selectedRunId: string | null;

  create: (question: string) => DeepResearchRun;
  selectRun: (id: string | null) => void;
  setStatus: (id: string, status: DeepResearchStatus) => void;
  setPlan: (id: string, plan: ResearchPlan) => void;
  setPendingStep: (id: string, stepId: string | null) => void;
  appendStepResult: (id: string, outcome: StepOutcome) => void;
  setReport: (id: string, report: ResearchReport) => void;
  setError: (id: string, error: string) => void;
  remove: (id: string) => void;
}

function patchRun(
  state: DeepResearchStoreState,
  id: string,
  patch: Partial<DeepResearchRun> | ((run: DeepResearchRun) => Partial<DeepResearchRun>),
): DeepResearchStoreState {
  const existing = state.runs[id];
  if (!existing) return state;
  const resolved = typeof patch === "function" ? patch(existing) : patch;
  return {
    ...state,
    runs: { ...state.runs, [id]: { ...existing, ...resolved, updatedAtMs: Date.now() } },
  };
}

export const useDeepResearchStore = create<DeepResearchStoreState>((set, get) => ({
  runs: {},
  order: [],
  selectedRunId: null,

  create: (question) => {
    const now = Date.now();
    const run: DeepResearchRun = {
      id: crypto.randomUUID(),
      question: question.trim(),
      status: "planning",
      plan: null,
      stepResults: [],
      pendingStepId: null,
      report: null,
      error: null,
      createdAtMs: now,
      updatedAtMs: now,
    };
    set((state) => ({
      runs: { ...state.runs, [run.id]: run },
      order: [run.id, ...state.order],
      selectedRunId: run.id,
    }));
    return run;
  },

  selectRun: (id) => set({ selectedRunId: id }),

  setStatus: (id, status) => set((state) => patchRun(state, id, { status })),

  setPlan: (id, plan) => set((state) => patchRun(state, id, { plan })),

  setPendingStep: (id, stepId) => set((state) => patchRun(state, id, { pendingStepId: stepId })),

  appendStepResult: (id, outcome) =>
    set((state) =>
      patchRun(state, id, (run) => ({
        stepResults: [...run.stepResults, outcome],
        pendingStepId: run.pendingStepId === outcome.step.id ? null : run.pendingStepId,
      })),
    ),

  setReport: (id, report) => set((state) => patchRun(state, id, { report, status: "done", pendingStepId: null })),

  setError: (id, error) => set((state) => patchRun(state, id, { error, status: "error", pendingStepId: null })),

  remove: (id) =>
    set((state) => {
      if (!state.runs[id]) return state;
      const runs = { ...state.runs };
      delete runs[id];
      return {
        runs,
        order: state.order.filter((entry) => entry !== id),
        selectedRunId: get().selectedRunId === id ? null : get().selectedRunId,
      };
    }),
}));

/** Selector: every run in display order (newest first). */
export function selectDeepResearchRuns(state: DeepResearchStoreState): DeepResearchRun[] {
  return state.order.map((id) => state.runs[id]).filter((run): run is DeepResearchRun => Boolean(run));
}
