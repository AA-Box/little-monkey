import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

/** Mirrors the camelCase `CheckpointInfo` payload returned by the Rust
 * `checkpoint_list` command (src-tauri/src/checkpoints.rs). `files` is a
 * count, not the touched paths — enough for the timeline's "N files
 * changed" row without shipping every path over just to render a number. */
export interface CheckpointInfo {
  id: string;
  createdAtMs: number;
  sessionId: string;
  anchorIndex: number;
  label: string;
  files: number;
  shellRan: boolean;
  reverted: boolean;
  /** True once a revert on this checkpoint actually recorded a redo backup —
   * gates whether "Re-apply" is worth offering at all. */
  reapplyable: boolean;
}

interface CheckpointStoreState {
  /** Session id -> its checkpoints, newest first, as of the last successful
   * `refresh` for that session. Keyed per session (not a single flat list)
   * because the split pane can have two different sessions' timelines open
   * at once. */
  bySession: Record<string, CheckpointInfo[]>;
  loadingSessions: Record<string, boolean>;
  errorsBySession: Record<string, string | null>;
  /**
   * Re-fetches `sessionId`'s checkpoint list from the backend and replaces
   * the cached entry. Safe to call speculatively — e.g. after every turn
   * end, or after a revert/reapply — even when no `CheckpointTimeline` panel
   * is open for that session: it's a cheap directory scan, and the result
   * just sits in cache until a panel actually reads it. Failures are
   * recorded in `errorsBySession` rather than thrown, so a fire-and-forget
   * caller (like `agentLoop.ts`'s turn-end hook) never needs a `.catch`.
   */
  refresh: (sessionId: string) => Promise<void>;
}

export const useCheckpointStore = create<CheckpointStoreState>((set) => ({
  bySession: {},
  loadingSessions: {},
  errorsBySession: {},

  refresh: async (sessionId) => {
    set((state) => ({
      loadingSessions: { ...state.loadingSessions, [sessionId]: true },
      errorsBySession: { ...state.errorsBySession, [sessionId]: null },
    }));
    try {
      const list = await invoke<CheckpointInfo[]>("checkpoint_list", { sessionId });
      set((state) => ({
        bySession: { ...state.bySession, [sessionId]: list },
        loadingSessions: { ...state.loadingSessions, [sessionId]: false },
      }));
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      set((state) => ({
        loadingSessions: { ...state.loadingSessions, [sessionId]: false },
        errorsBySession: { ...state.errorsBySession, [sessionId]: message },
      }));
    }
  },
}));

export default useCheckpointStore;
