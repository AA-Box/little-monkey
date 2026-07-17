import { create } from "zustand";

/**
 * Multi-Agent Debate and Red-Team Mode (ROADMAP.md Phase 7, item 26): six
 * fixed named-role subagents (Proposer, Critic, Security, Reliability, Cost,
 * User Advocate) each form an INDEPENDENT position on a decision question —
 * none of them ever sees another role's answer while forming its own (see
 * `../lib/debateRunner.ts`'s `runDebate`) — and a final synthesis pass
 * explicitly lists each role's objections plus why the winning path
 * addresses or overrides them, rather than flattening six perspectives into
 * one answer too early (the roadmap's own acceptance criterion).
 *
 * Deliberately its own store (not folded into `sideTaskStore.ts` or
 * `subagentStore.ts`): a debate's unit of work is one whole run with six
 * fixed roles plus a synthesis, not a single open-ended agent loop, so its
 * state shape (per-role position/objections, a distinct synthesis object)
 * doesn't fit either of those. Not persisted, mirroring `sideTaskStore.ts`'s
 * own reasoning: a debate is transient, in-window work — closing the app
 * mid-run is the same as hitting Cancel.
 */

export type DebateRoleId =
  | "proposer"
  | "critic"
  | "security"
  | "reliability"
  | "cost"
  | "user_advocate";

export type DebateRoleStatus = "pending" | "running" | "completed" | "failed" | "cancelled";

export interface DebatePosition {
  roleId: DebateRoleId;
  /** Display label, frozen at creation (mirrors `ResolvedTarget`'s "resolved
   * once" invariant elsewhere) so a later i18n locale switch never rewrites
   * an already-rendered transcript's role names mid-run. */
  roleLabel: string;
  status: DebateRoleStatus;
  /** The role's 2-4 sentence stance, parsed out of its reply. Null until the
   * role completes. */
  position: string | null;
  /** Objections/risks/tradeoffs the role itself raised from its own lens —
   * preserved verbatim, never merged into `position`, so the panel can show
   * "what this role argued for" and "what this role warned about"
   * separately, per the roadmap's "preserve disagreements" acceptance. */
  objections: string[];
  /** The role's full raw reply, kept for a "show raw" fallback if parsing
   * the POSITION/OBJECTIONS shape failed. */
  rawOutput: string;
  error: string | null;
  startedAt: number | null;
  completedAt: number | null;
}

/** One synthesis-listed objection and how the final recommendation handles
 * it — the panel renders one row per entry so a reader can see every
 * surfaced objection next to its disposition, not just a flattened answer. */
export interface DebateObjectionHandling {
  roleId: DebateRoleId | null;
  roleLabel: string;
  objection: string;
  resolution: string;
}

export interface DebateSynthesis {
  recommendation: string;
  objectionHandling: DebateObjectionHandling[];
  tradeoffs: string;
  whyThisWon: string;
  /** True when the synthesis model's reply could not be parsed into the
   * structured shape above — `recommendation` then holds the raw reply
   * verbatim (never silently dropped) and the panel shows a notice instead
   * of pretending the structure was honored. */
  parseFailed: boolean;
  raw: string;
}

export type DebateStatus = "idle" | "running" | "completed" | "failed" | "cancelled";

export interface DebateRun {
  id: string;
  question: string;
  status: DebateStatus;
  modelLabel: string;
  createdAt: number;
  startedAt: number | null;
  completedAt: number | null;
  durationMs: number | null;
  error: string | null;
  positions: DebatePosition[];
  synthesis: DebateSynthesis | null;
}

interface DebateStoreState {
  runs: Record<string, DebateRun>;
  /** Insertion order, newest first. */
  order: string[];
  activeRunId: string | null;

  create: (id: string, question: string, initialPositions: DebatePosition[]) => DebateRun;
  setModelLabel: (id: string, modelLabel: string) => void;
  markRunning: (id: string) => void;
  updatePosition: (id: string, roleId: DebateRoleId, patch: Partial<DebatePosition>) => void;
  setSynthesis: (id: string, synthesis: DebateSynthesis) => void;
  finish: (id: string, status: "completed" | "failed" | "cancelled", error: string | null) => void;
  selectRun: (id: string | null) => void;
  remove: (id: string) => void;
}

function patchRun(
  state: DebateStoreState,
  id: string,
  patch: Partial<DebateRun> | ((run: DebateRun) => Partial<DebateRun>),
): DebateStoreState {
  const existing = state.runs[id];
  if (!existing) return state;
  const resolved = typeof patch === "function" ? patch(existing) : patch;
  return { ...state, runs: { ...state.runs, [id]: { ...existing, ...resolved } } };
}

export const useDebateStore = create<DebateStoreState>((set) => ({
  runs: {},
  order: [],
  activeRunId: null,

  create: (id, question, initialPositions) => {
    const run: DebateRun = {
      id,
      question,
      status: "idle",
      modelLabel: "Resolving model…",
      createdAt: Date.now(),
      startedAt: null,
      completedAt: null,
      durationMs: null,
      error: null,
      positions: initialPositions,
      synthesis: null,
    };
    set((state) => ({
      runs: { ...state.runs, [id]: run },
      order: [id, ...state.order],
      activeRunId: id,
    }));
    return run;
  },

  setModelLabel: (id, modelLabel) => set((state) => patchRun(state, id, { modelLabel })),

  markRunning: (id) =>
    set((state) => patchRun(state, id, (run) => ({ status: "running", startedAt: run.startedAt ?? Date.now() }))),

  updatePosition: (id, roleId, patch) =>
    set((state) =>
      patchRun(state, id, (run) => {
        const existingIndex = run.positions.findIndex((position) => position.roleId === roleId);
        if (existingIndex === -1) return run;
        const positions = [...run.positions];
        positions[existingIndex] = { ...positions[existingIndex], ...patch };
        return { positions };
      }),
    ),

  setSynthesis: (id, synthesis) => set((state) => patchRun(state, id, { synthesis })),

  finish: (id, status, error) =>
    set((state) =>
      patchRun(state, id, (run) => {
        const completedAt = Date.now();
        return {
          status,
          error,
          completedAt,
          durationMs: run.startedAt !== null ? completedAt - run.startedAt : null,
        };
      }),
    ),

  selectRun: (id) => set({ activeRunId: id }),

  remove: (id) =>
    set((state) => {
      if (!state.runs[id]) return state;
      const runs = { ...state.runs };
      delete runs[id];
      return {
        runs,
        order: state.order.filter((entry) => entry !== id),
        activeRunId: state.activeRunId === id ? null : state.activeRunId,
      };
    }),
}));

/** Selector: every debate run in display order (newest first). */
export function selectDebateRuns(state: DebateStoreState): DebateRun[] {
  return state.order.map((id) => state.runs[id]).filter((run): run is DebateRun => Boolean(run));
}
