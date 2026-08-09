import { create } from "zustand";

/**
 * Live progress of the ACTIVE chat turn in each session — what the chat's
 * status line ("✳ 1m 30s · 181.1k tokens · thinking…") renders from.
 *
 * Deliberately a separate transient store (mirroring `subagentStore`'s
 * posture) rather than more fields on `sessionStore`: this state changes on
 * every usage event and tool call of a turn, and `sessionStore` persists —
 * none of this should ever hit disk or survive a restart. Entries are keyed
 * by sessionId because the app allows one running turn per session (see
 * `agentLoop.ts`'s `turnControllers` invariant) and split view can run two
 * sessions side by side.
 */
export interface TurnStatus {
  sessionId: string;
  /** When `begin` registered the turn — drives the ticking elapsed label. */
  startedAt: number;
  /** Running total of `usage.totalTokens` across every model attempt this
   * turn has made so far (initial stream, tool-round follow-ups, failovers,
   * context-trim summarization) — accumulated, not "most recent", for the
   * same reason `SubagentRun.usage` accumulates: the user-facing question is
   * "how much has this whole turn cost so far". */
  totalTokens: number;
  /** Characters streamed by the CURRENT attempt since its last exact usage
   * report, from which the status line derives a live estimate. An
   * OpenAI-compatible endpoint only reports `usage` in the stream's final
   * chunk, so without this the token count sits at whatever the previous
   * round ended on for the entire time an answer is being written — exactly
   * when the user is watching it. Reset by `addTokens`, since the exact
   * number supersedes the guess. */
  streamedChars: number;
  /** Short label of the tool call currently executing (e.g. `read_file`),
   * or `""` while the model itself is streaming/thinking — the status
   * line's trailing word switches on this. */
  activity: string;
  /** When this turn last showed a sign of life (registered, reported usage,
   * or crossed a tool boundary) — the status line escalates "thinking…" to
   * "still thinking…" off SILENCE (now − lastEventAt), not total turn age,
   * so a long multi-round turn isn't permanently branded "still thinking". */
  lastEventAt: number;
}

interface TurnStatusStoreState {
  /** At most one entry per session — created by `begin`, removed by `end`.
   * Transient, never persisted. */
  turns: Record<string, TurnStatus>;
  /** Registers the turn the instant `runAgentTurn` accepts it — before any
   * streaming — so the status line can start ticking immediately. */
  begin: (sessionId: string) => void;
  /** Adds one attempt's reported `totalTokens` onto the running total.
   * No-ops if no turn is registered — purely defensive; every current
   * caller runs inside a registered turn (the risk judge, which doesn't,
   * opts out via `attemptStream`'s `recordTurnStatusTokens`). */
  addTokens: (sessionId: string, totalTokens: number) => void;
  /** Adds streamed characters onto the current attempt's running estimate.
   * Callers batch these (see `turnEngine.ts`) rather than reporting every
   * delta — a store write per token would re-render the transcript on every
   * token. No-ops if no turn is registered. */
  noteStreamedChars: (sessionId: string, chars: number) => void;
  /** Sets the currently-executing tool label, or `""` when control returns
   * to the model. No-ops if no turn is registered. */
  setActivity: (sessionId: string, activity: string) => void;
  /** Removes the entry — called from `runAgentTurn`'s finally, so it always
   * runs no matter how the turn ended. */
  end: (sessionId: string) => void;
}

export const useTurnStatusStore = create<TurnStatusStoreState>((set) => ({
  turns: {},

  begin: (sessionId) => {
    set((state) => {
      const now = Date.now();
      return {
        turns: {
          ...state.turns,
          [sessionId]: { sessionId, startedAt: now, totalTokens: 0, streamedChars: 0, activity: "", lastEventAt: now },
        },
      };
    });
  },

  addTokens: (sessionId, totalTokens) => {
    set((state) => {
      const existing = state.turns[sessionId];
      if (!existing) return state;
      return {
        turns: {
          ...state.turns,
          [sessionId]: {
            ...existing,
            totalTokens: existing.totalTokens + totalTokens,
            streamedChars: 0,
            lastEventAt: Date.now(),
          },
        },
      };
    });
  },

  noteStreamedChars: (sessionId, chars) => {
    set((state) => {
      const existing = state.turns[sessionId];
      if (!existing) return state;
      return {
        turns: {
          ...state.turns,
          [sessionId]: { ...existing, streamedChars: existing.streamedChars + chars, lastEventAt: Date.now() },
        },
      };
    });
  },

  setActivity: (sessionId, activity) => {
    set((state) => {
      const existing = state.turns[sessionId];
      if (!existing) return state;
      return { turns: { ...state.turns, [sessionId]: { ...existing, activity, lastEventAt: Date.now() } } };
    });
  },

  end: (sessionId) => {
    set((state) => {
      if (!state.turns[sessionId]) return state;
      const next = { ...state.turns };
      delete next[sessionId];
      return { turns: next };
    });
  },
}));

/** Rough characters-per-token used to turn streamed text into the live
 * estimate below. */
// ponytail: a fixed 4:1 ratio, not a real tokenizer — close enough for a
// status line that the exact `usage` overwrites moments later, and it costs
// no bundle weight. Swap in a real tokenizer only if the estimate ever needs
// to be trusted for anything but display.
const CHARS_PER_TOKEN = 4;

/** Tokens to show for a turn right now: everything its finished attempts
 * reported, plus an estimate of what the in-flight one has streamed since.
 * Returns `estimated: true` while any part of the number is a guess, which
 * the status line marks with a `~`. */
export function liveTurnTokens(status: TurnStatus): { tokens: number; estimated: boolean } {
  const estimate = Math.floor(status.streamedChars / CHARS_PER_TOKEN);
  return { tokens: status.totalTokens + estimate, estimated: estimate > 0 };
}

/** Zustand selector: the active turn's live status for one session, or
 * `undefined` when no turn is running there. */
export function selectTurnStatus(sessionId: string) {
  return (state: TurnStatusStoreState): TurnStatus | undefined => state.turns[sessionId];
}
