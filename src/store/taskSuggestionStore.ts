import { create } from "zustand";

/**
 * Spawned-task suggestions — the chips Claude Code desktop drops under the
 * transcript when the model notices work worth doing that would bloat the
 * current change (dead code, a stale doc, a missing test, a security issue
 * spotted in passing). The model calls the `spawn_task` tool; a chip appears;
 * one click spins that suggestion off into its OWN chat session, with the
 * current turn continuing uninterrupted.
 *
 * Three surfaces, three meanings, deliberately not merged:
 * - a BACKGROUND task (`backgroundShellStore.ts`, `subagentStore.ts`) is
 *   headless work already running;
 * - a SIDE TASK (`sideTaskStore.ts`) is a parallel conversation the user
 *   opened and can talk to;
 * - a task SUGGESTION (this store) is not running at all. It is an offer.
 *   Nothing here starts a model call until the user clicks the chip — that
 *   is the entire safety property of this feature, and why `spawn_task`
 *   needs no permission prompt.
 *
 * Not persisted: a suggestion is about the change currently on screen, and a
 * stale one a week later is noise rather than a reminder.
 */

export type TaskSuggestionStatus = "pending" | "started" | "dismissed";

export interface TaskSuggestion {
  id: string;
  /** The chat session whose turn proposed this — chips only ever render
   * under the transcript they came from. */
  sessionId: string;
  /** Imperative action phrase, e.g. "Remove dead config option". */
  title: string;
  /** One or two plain sentences shown on hover: what the spun-off session
   * would do and why. */
  tldr: string;
  /** The self-contained instruction the spun-off session starts from. Must
   * stand alone — the new session has none of this conversation's context. */
  prompt: string;
  status: TaskSuggestionStatus;
  createdAt: number;
  /** The session this suggestion was spun off into, once started. */
  spawnedSessionId: string | null;
}

export interface CreateTaskSuggestionParams {
  sessionId: string;
  title: string;
  tldr: string;
  prompt: string;
}

/** Bounds on model-supplied chip text, so a chatty model can't push a wall
 * of text into the composer area. The prompt is capped far higher — it is
 * the actual instruction, not a label. */
export const MAX_SUGGESTION_TITLE_CHARS = 60;
export const MAX_SUGGESTION_TLDR_CHARS = 240;
export const MAX_SUGGESTION_PROMPT_CHARS = 8_000;
/** Most chips shown for one session at once — older pending chips fall off
 * rather than stacking into a wall above the composer. */
export const MAX_VISIBLE_SUGGESTIONS = 3;

function clamp(value: string, maxChars: number): string {
  const normalized = value.replace(/\s+/g, " ").trim();
  return normalized.length <= maxChars ? normalized : `${normalized.slice(0, maxChars - 1)}…`;
}

interface TaskSuggestionStoreState {
  suggestions: Record<string, TaskSuggestion>;
  /** Newest first. */
  order: string[];
  create: (params: CreateTaskSuggestionParams) => TaskSuggestion;
  markStarted: (id: string, spawnedSessionId: string) => void;
  dismiss: (id: string) => void;
  /** Puts a dismissed chip back, for `checkpoint_reapply` — see
   * `checkpointCompensation.ts`. Deliberately refuses a chip the user has
   * already *started*: that spun off a real session, and quietly reverting its
   * status would misreport work that actually happened. */
  restore: (id: string) => void;
  clearForSession: (sessionId: string) => void;
}

export const useTaskSuggestionStore = create<TaskSuggestionStoreState>((set) => ({
  suggestions: {},
  order: [],

  create: (params) => {
    const suggestion: TaskSuggestion = {
      id: crypto.randomUUID(),
      sessionId: params.sessionId,
      title: clamp(params.title, MAX_SUGGESTION_TITLE_CHARS) || "Follow-up task",
      tldr: clamp(params.tldr, MAX_SUGGESTION_TLDR_CHARS),
      // Newlines survive here (unlike the labels above): the prompt is an
      // instruction the spun-off session reads, not a chip caption.
      prompt: params.prompt.trim().slice(0, MAX_SUGGESTION_PROMPT_CHARS),
      status: "pending",
      createdAt: Date.now(),
      spawnedSessionId: null,
    };
    set((state) => ({
      suggestions: { ...state.suggestions, [suggestion.id]: suggestion },
      order: [suggestion.id, ...state.order],
    }));
    return suggestion;
  },

  markStarted: (id, spawnedSessionId) =>
    set((state) => {
      const existing = state.suggestions[id];
      if (!existing) return state;
      return {
        suggestions: { ...state.suggestions, [id]: { ...existing, status: "started", spawnedSessionId } },
      };
    }),

  dismiss: (id) =>
    set((state) => {
      const existing = state.suggestions[id];
      if (!existing) return state;
      return { suggestions: { ...state.suggestions, [id]: { ...existing, status: "dismissed" } } };
    }),

  restore: (id) =>
    set((state) => {
      const existing = state.suggestions[id];
      if (!existing || existing.status !== "dismissed") return state;
      return { suggestions: { ...state.suggestions, [id]: { ...existing, status: "pending" } } };
    }),

  clearForSession: (sessionId) =>
    set((state) => {
      const suggestions = { ...state.suggestions };
      const order = state.order.filter((id) => {
        if (suggestions[id]?.sessionId !== sessionId) return true;
        delete suggestions[id];
        return false;
      });
      return { suggestions, order };
    }),
}));

/** The chips actually shown under a session's transcript: pending only
 * (started and dismissed ones disappear), newest first, bounded. */
export function selectPendingSuggestions(sessionId: string) {
  return (state: TaskSuggestionStoreState): TaskSuggestion[] =>
    state.order
      .map((id) => state.suggestions[id])
      .filter((suggestion): suggestion is TaskSuggestion =>
        Boolean(suggestion) && suggestion.sessionId === sessionId && suggestion.status === "pending",
      )
      .slice(0, MAX_VISIBLE_SUGGESTIONS);
}
