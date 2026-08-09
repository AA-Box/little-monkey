import { create } from "zustand";

/** One phase's shape inside a live/persisted workflow run: the model-supplied
 * title plus the `subagentStore` keys of the agents dispatched under it —
 * the agents' own stats (status/tokens/tools/timing/transcript) live in
 * `subagentStore`/`ChatSession.subagentRunMeta` exactly like any other
 * subagent run; this store only holds the workflow's SHAPE. */
export interface WorkflowPhase {
  title: string;
  agents: { taskId: string; description: string }[];
}

export type WorkflowStatus = "running" | "done" | "error" | "cancelled";

/**
 * One `workflow` tool call's run — the named, phased counterpart of a plain
 * parallel `task` round (see `SubagentRun.groupId`). Keyed by the
 * originating `workflow` tool_call's own id (`runId`), for the same
 * transcript-correlation reason `SubagentRun` is keyed by `taskId` — see
 * that interface's doc comment. Transient, never persisted: restarted
 * sessions fall back to `ChatSession.workflowRunMeta`.
 */
export interface WorkflowRun {
  sessionId: string;
  runId: string;
  name: string;
  description: string;
  status: WorkflowStatus;
  startedAt: number;
  finishedAt?: number;
  phases: WorkflowPhase[];
  /** Index into `phases` of the phase currently executing — agents of later
   * phases haven't been dispatched yet and have no `subagentStore` entry. */
  activePhaseIndex: number;
}

interface WorkflowStoreState {
  runs: Record<string, WorkflowRun>;
  /** Registers a run as `'running'` on phase 0 — called once by
   * `runWorkflow` before any agent is dispatched. */
  start: (params: { sessionId: string; runId: string; name: string; description: string; phases: WorkflowPhase[] }) => void;
  /** Advances the active-phase pointer — called by `runWorkflow` as each
   * phase's agents begin. No-ops on an unknown `runId`, same defensive
   * posture as `subagentStore.recordToolCall`. */
  beginPhase: (runId: string, phaseIndex: number) => void;
  /** Marks a run terminal — called once, when `runWorkflow` is about to
   * return. */
  finish: (runId: string, status: "done" | "error" | "cancelled") => void;
  /** Drops every terminal run — the Background-tasks drawer's "Clear"
   * button, alongside `subagentStore.clearFinished`. */
  clearFinished: () => void;
}

export const useWorkflowStore = create<WorkflowStoreState>((set) => ({
  runs: {},

  start: ({ sessionId, runId, name, description, phases }) => {
    set((state) => ({
      runs: {
        ...state.runs,
        [runId]: { sessionId, runId, name, description, status: "running", startedAt: Date.now(), phases, activePhaseIndex: 0 },
      },
    }));
  },

  beginPhase: (runId, phaseIndex) => {
    set((state) => {
      const existing = state.runs[runId];
      if (!existing) return state;
      return { runs: { ...state.runs, [runId]: { ...existing, activePhaseIndex: phaseIndex } } };
    });
  },

  finish: (runId, status) => {
    set((state) => {
      const existing = state.runs[runId];
      if (!existing) return state;
      return { runs: { ...state.runs, [runId]: { ...existing, status, finishedAt: Date.now() } } };
    });
  },

  clearFinished: () => {
    set((state) => ({
      runs: Object.fromEntries(Object.entries(state.runs).filter(([, run]) => run.status === "running")),
    }));
  },
}));

/** Every run this window session has seen, newest first — the
 * Background-tasks drawer's workflow entries. Fresh array per call; wrap in
 * `useShallow` at the subscription site, same as `selectSubagentRunList`. */
export function selectWorkflowRunList(state: WorkflowStoreState): WorkflowRun[] {
  return Object.values(state.runs).sort((a, b) => b.startedAt - a.startedAt);
}
