/**
 * Agent-Ready Spec Scorer store (ROADMAP.md Phase 7, item 4) — caches the
 * advisory `specScorer.ts` result per Issue-to-PR run so `IssueToPrPanel.tsx`
 * can show it without re-scoring on every re-render, and owns the actual
 * `resolveTarget`/`effortForTarget`/`attemptStream` wiring `specScorer.ts`'s
 * `scoreSpec` needs a `callModel` closure for — exactly the same
 * `resolveTarget()` -> `effortForTarget(target)` -> one-shot, tool-less,
 * non-recording `attemptStream(...)` shape `agentLoop.ts`'s
 * `compactSessionNow`/`sendForSummary` and its risk-annotation `classify`
 * closure both use for their own one-shot judge/summary calls.
 *
 * Purely advisory and purely additive: nothing here ever blocks
 * `issueToPrStore.ts`'s `start`/`driveRun` — a run proceeds identically
 * whether this store has scored it, is still scoring it, or failed to score
 * it at all.
 */
import { create } from "zustand";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import { scoreSpec, type SpecScore } from "../lib/specScorer";
import { effortForTarget } from "./modelStore";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export type SpecScorerStatus = "idle" | "loading" | "done" | "error";

/** In-flight abort handles, keyed by run id — deliberately NOT part of the
 * zustand state, exactly like `issueToPrStore.ts`'s `controllers` map (an
 * `AbortController` isn't a value React/zustand needs to react to). Process-
 * lifetime by design, which is why tests reset it explicitly. */
const controllers = new Map<string, AbortController>();

/** Test-only: clears in-flight scoring controllers — see the comment on
 * `controllers` above and `issueToPrStore.ts`'s identical test helper. */
export function __resetSpecScorerControllersForTests(): void {
  for (const controller of controllers.values()) controller.abort();
  controllers.clear();
}

interface SpecScorerState {
  scoresByRun: Record<string, SpecScore | null>;
  statusByRun: Record<string, SpecScorerStatus>;
  errorByRun: Record<string, string | null>;

  /** Scores `issueTitle`/`issueBody` under `runId` if it hasn't already been
   * scored (or isn't already in flight) — safe to call on every selection
   * change/render, since it's a no-op once a run has a cached result.
   * Never throws: every failure lands as `statusByRun[runId] === "error"`. */
  scoreRun: (runId: string, issueTitle: string, issueBody: string) => Promise<void>;
  /** Forces a fresh score for `runId`, discarding any cached result — used
   * by the panel's manual "Re-check" affordance. */
  rescoreRun: (runId: string, issueTitle: string, issueBody: string) => Promise<void>;
  clearRun: (runId: string) => void;
}

export const useSpecScorerStore = create<SpecScorerState>((set, get) => {
  const run = async (runId: string, issueTitle: string, issueBody: string): Promise<void> => {
    controllers.get(runId)?.abort();
    const controller = new AbortController();
    controllers.set(runId, controller);
    set((state) => ({
      statusByRun: { ...state.statusByRun, [runId]: "loading" },
      errorByRun: { ...state.errorByRun, [runId]: null },
    }));

    try {
      const target = await resolveTarget();
      const effort = effortForTarget(target);
      const score = await scoreSpec(
        issueTitle,
        issueBody,
        (messages, signal) =>
          // `tools: []` (no tool-calling), `recordUsage: false` (this is a
          // background advisory call with no chat bubble/session of its own
          // to attribute tokens to — the same reasoning `subagent.ts`'s
          // per-subagent attempts use), no durable-run id (there is no Run
          // Capsule for an advisory score).
          attemptStream(target, messages, [], signal, effort, `spec-score:${runId}`, undefined, false),
        controller.signal,
      );
      // A stale controller (superseded by a later `run`/`rescoreRun` call
      // for the same runId while this one was in flight) must never clobber
      // the newer call's result.
      if (controllers.get(runId) !== controller) return;
      set((state) => ({
        scoresByRun: { ...state.scoresByRun, [runId]: score },
        statusByRun: { ...state.statusByRun, [runId]: score ? "done" : "error" },
        errorByRun: {
          ...state.errorByRun,
          [runId]: score ? null : "Could not score this issue right now.",
        },
      }));
    } catch (error) {
      if (controllers.get(runId) !== controller) return;
      set((state) => ({
        statusByRun: { ...state.statusByRun, [runId]: "error" },
        errorByRun: { ...state.errorByRun, [runId]: errorText(error) },
      }));
    } finally {
      if (controllers.get(runId) === controller) controllers.delete(runId);
    }
  };

  return {
    scoresByRun: {},
    statusByRun: {},
    errorByRun: {},

    scoreRun: async (runId, issueTitle, issueBody) => {
      if (get().statusByRun[runId]) return;
      await run(runId, issueTitle, issueBody);
    },

    rescoreRun: (runId, issueTitle, issueBody) => run(runId, issueTitle, issueBody),

    clearRun: (runId) =>
      set((state) => {
        controllers.get(runId)?.abort();
        controllers.delete(runId);
        const scoresByRun = { ...state.scoresByRun };
        const statusByRun = { ...state.statusByRun };
        const errorByRun = { ...state.errorByRun };
        delete scoresByRun[runId];
        delete statusByRun[runId];
        delete errorByRun[runId];
        return { scoresByRun, statusByRun, errorByRun };
      }),
  };
});
