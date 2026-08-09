import { create } from "zustand";
import type { ChatMessage } from "../lib/llamaClient";
import type { UsageInfo } from "./usageStore";

/** Mirrors `runSubagentTask`'s own possible outcomes (see subagent.ts):
 * `'running'` while the child's model->tools->model loop is still going,
 * `'done'` once it produced a final report, `'error'` for a stream failure
 * or the `MAX_SUBAGENT_ITERATIONS` cap, `'cancelled'` when the parent's Stop
 * button fired mid-run. */
export type SubagentStatus = "running" | "done" | "error" | "cancelled";

/**
 * Live status of one in-flight or just-finished subagent run, keyed by
 * `taskId` in the store below. `taskId` here is the ORIGINATING `task`
 * tool_call's `ToolCall.id` — not `runSubagentTask`'s own Rust-facing
 * `crypto.randomUUID()` turn id (see that function's `RunSubagentTaskParams`
 * doc comment) — because that's the only identifier `MessageList.tsx`'s
 * `buildTimeline` actually has on hand when it walks the persisted
 * transcript and needs to correlate a rendered `SubagentRow` with this
 * store's live entry. The two ids serve deliberately different purposes and
 * must never be conflated: the Rust turn id scopes cancellation/permission
 * prompts (security-relevant, must stay globally unique per concurrent
 * turn), while this key is purely a UI correlation handle.
 */
export interface SubagentRun {
  sessionId: string;
  taskId: string;
  /** The run's Rust-facing `crypto.randomUUID()` turn id — what
   * `cancelSubagentRun` (subagent.ts) is keyed by. NOT `taskId`: provider-
   * fallback tool-call ids (`call_0`, see llamaClient.ts) can collide across
   * two concurrent turns' subagents, and cancelling by a collided key would
   * abort the wrong run. */
  cancelId: string;
  /** Shared id linking the parallel `task` calls of one assistant round —
   * `undefined` for a lone call. Generated per round by `agentLoop.ts` (a
   * fresh UUID, never a tool-call id: provider-fallback ids like `call_0`
   * repeat across rounds and would merge unrelated groups). The
   * Background-tasks drawer groups same-`groupId` runs into one card, the
   * panel counterpart of the chat's `SubagentGroupCard`. */
  groupId?: string;
  /** Set when this run is one agent of a `workflow` tool call (the owning
   * `WorkflowRun.runId`) — the Background-tasks drawer renders it inside
   * that workflow's card and excludes it from the plain agent list. */
  workflowRunId?: string;
  description: string;
  profile: "explore" | "code";
  status: SubagentStatus;
  /** When `start` registered this run — drives the elapsed-time column in
   * the Background-tasks drawer and the grouped-agents card. Wall-clock
   * only; never persisted (the whole store is transient), so restarted
   * sessions simply show no timing, same as they show no token usage. */
  startedAt: number;
  /** When `finish` marked this run terminal — `undefined` while running. */
  finishedAt?: number;
  /** Short label for the child's most recent action, e.g. `grep("resolveTarget")` — blank until the first tool call. */
  lastActivity: string;
  toolCallCount: number;
  /** Running total of token usage across every model attempt this subagent's
   * own loop has made so far (slice 4) — accumulated, not "most recent",
   * unlike `useUsageStore`'s per-session number: a subagent's own internal
   * iterations aren't separate "turns" from the parent's perspective, so
   * summing gives "how many tokens did this whole delegated task cost" that
   * a per-attempt-only number wouldn't. `undefined` until the child's first
   * `attemptStream` call reports a `usage` event (some providers/local
   * models never do). Never written to `useUsageStore` itself — see
   * `attemptStream`'s `recordUsage: false` doc comment for why child usage
   * must never touch the PARENT session's own context-usage ring; this
   * field is a parallel, subagent-scoped accounting the parent's ring never
   * sees. */
  usage?: UsageInfo;
  /** The child's own growing transcript (seed user prompt, then
   * assistant/tool messages as its loop progresses) — used to render the
   * expandable mini-transcript. Never written into `sessionStore`'s
   * `messages` (the wire payload sent to the model) — see
   * `subagent.test.ts`'s wire-payload-isolation test for the invariant this
   * whole field must never violate. */
  liveMessages: ChatMessage[];
}

interface SubagentStoreState {
  /** All runs this window session has seen, keyed by `taskId` — transient,
   * NEVER persisted (deliberately absent from any `persist`/hydrate path):
   * on restart, historical rows fall back to `ChatSession.subagentRuns`
   * (see sessionStore.ts) for their mini-transcript instead. */
  runs: Record<string, SubagentRun>;
  /** Registers a new run as `'running'` with an empty activity log — called
   * once by `runSubagentTask` right before it starts the child's loop. */
  start: (params: { sessionId: string; taskId: string; cancelId: string; groupId?: string; workflowRunId?: string; description: string; profile: "explore" | "code" }) => void;
  /** Updates `lastActivity` and bumps `toolCallCount` by one — called once
   * per child tool call `runSubagentTask` is about to execute. No-ops if
   * `taskId` was never `start`-ed (defensive; should not happen). */
  recordToolCall: (taskId: string, activity: string) => void;
  /** Appends one message to `taskId`'s `liveMessages` — called by
   * `runSubagentTask` for every assistant/tool message the child's loop
   * produces, so the mini-transcript can render mid-run. */
  appendMessage: (taskId: string, message: ChatMessage) => void;
  /** Adds one `attemptStream` call's reported usage onto `taskId`'s running
   * `usage` total (see that field's own doc comment for why this
   * accumulates rather than replaces) — called once per iteration of
   * `runSubagentTask`'s loop that reports a `usage` event. No-ops if
   * `taskId` was never `start`-ed, same defensive posture as
   * `recordToolCall`. */
  accumulateUsage: (taskId: string, usage: UsageInfo) => void;
  /** Marks a run terminal (`'done' | 'error' | 'cancelled'`) — called once,
   * when `runSubagentTask` is about to return. */
  finish: (taskId: string, status: "done" | "error" | "cancelled") => void;
  /** Drops every terminal run — the Background-tasks drawer's "Clear"
   * button. Running entries are untouched; the cleared runs' transcripts
   * remain in `ChatSession.subagentRuns`, so inline `SubagentRow`s keep
   * working — this only empties the drawer's Finished list. */
  clearFinished: () => void;
}

export const useSubagentStore = create<SubagentStoreState>((set) => ({
  runs: {},

  start: ({ sessionId, taskId, cancelId, groupId, workflowRunId, description, profile }) => {
    set((state) => ({
      runs: {
        ...state.runs,
        [taskId]: { sessionId, taskId, cancelId, groupId, workflowRunId, description, profile, status: "running", startedAt: Date.now(), lastActivity: "", toolCallCount: 0, usage: undefined, liveMessages: [] },
      },
    }));
  },

  recordToolCall: (taskId, activity) => {
    set((state) => {
      const existing = state.runs[taskId];
      if (!existing) return state;
      return {
        runs: { ...state.runs, [taskId]: { ...existing, lastActivity: activity, toolCallCount: existing.toolCallCount + 1 } },
      };
    });
  },

  appendMessage: (taskId, message) => {
    set((state) => {
      const existing = state.runs[taskId];
      if (!existing) return state;
      return { runs: { ...state.runs, [taskId]: { ...existing, liveMessages: [...existing.liveMessages, message] } } };
    });
  },

  accumulateUsage: (taskId, usage) => {
    set((state) => {
      const existing = state.runs[taskId];
      if (!existing) return state;
      const prior = existing.usage;
      const merged: UsageInfo = prior
        ? {
            promptTokens: prior.promptTokens + usage.promptTokens,
            completionTokens: prior.completionTokens + usage.completionTokens,
            totalTokens: prior.totalTokens + usage.totalTokens,
          }
        : usage;
      return { runs: { ...state.runs, [taskId]: { ...existing, usage: merged } } };
    });
  },

  finish: (taskId, status) => {
    set((state) => {
      const existing = state.runs[taskId];
      if (!existing) return state;
      return { runs: { ...state.runs, [taskId]: { ...existing, status, finishedAt: Date.now() } } };
    });
  },

  clearFinished: () => {
    set((state) => ({
      runs: Object.fromEntries(Object.entries(state.runs).filter(([, run]) => run.status === "running")),
    }));
  },
}));

/** Zustand selector: one run's live status, or `undefined` if `taskId` was
 * never registered this window session (e.g. app restart) — callers fall
 * back to `ChatSession.subagentRuns` in that case. */
export function selectSubagentRun(taskId: string) {
  return (state: SubagentStoreState): SubagentRun | undefined => state.runs[taskId];
}

/** Count of subagent runs still in flight — combined with
 * `selectRunningSideTaskCount` to drive the chat's "N running tasks" chip
 * and the Background-tasks drawer badge. */
export function selectRunningSubagentCount(state: SubagentStoreState): number {
  return Object.values(state.runs).reduce((count, run) => (run.status === "running" ? count + 1 : count), 0);
}

/** Every run this window session has seen, newest first — the
 * Background-tasks drawer's subagent entries. Builds a fresh array per call:
 * wrap in `useShallow` at the subscription site, same as the side-task list
 * selectors. */
export function selectSubagentRunList(state: SubagentStoreState): SubagentRun[] {
  return Object.values(state.runs).sort((a, b) => b.startedAt - a.startedAt);
}
