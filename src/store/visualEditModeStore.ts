import { create } from "zustand";

import { proposeVisualEdit, writeVisualEditToDisk, type VisualEditElement } from "../lib/visualEditMode";
import { errorMessage } from "../lib/errors";

/**
 * Visual Design Edit Mode (ROADMAP.md Phase 7) — holds every visual edit the
 * user has started this session: the element they picked (via Browser
 * Workbench's own annotate flow), their plain-text description, the
 * before/after screenshots, and the generated source patch, through
 * pending -> accepted/rejected. Deliberately NOT persisted across restarts,
 * same posture as `sideTaskStore.ts` — a visual edit is transient in-session
 * work; what survives is only ever the real file write an Accept performs.
 */

export type VisualEditStatus = "generating" | "pending" | "accepted" | "rejected" | "error";

export interface VisualEditScreenshotRef {
  path: string;
  dataUrl: string;
}

export interface VisualEdit {
  id: string;
  createdAt: number;
  /** Browser Workbench session this element was captured from — used to
   * re-capture an "after" screenshot post-accept. */
  sessionId: string;
  pageUrl: string;
  description: string;
  element: VisualEditElement;
  beforeScreenshot: VisualEditScreenshotRef | null;
  afterScreenshot: VisualEditScreenshotRef | null;
  status: VisualEditStatus;
  targetFile: string | null;
  oldContent: string | null;
  newContent: string | null;
  unifiedDiff: string | null;
  summary: string | null;
  error: string | null;
}

export interface StartVisualEditParams {
  sessionId: string;
  pageUrl: string;
  description: string;
  element: VisualEditElement;
  beforeScreenshot: VisualEditScreenshotRef | null;
}

interface VisualEditModeState {
  edits: Record<string, VisualEdit>;
  /** Insertion order, newest first — the order the panel lists edits in. */
  order: string[];

  /** Creates a new pending edit (status `generating`) and kicks off the
   * proposal asynchronously, WITHOUT awaiting it — mirrors
   * `sideTaskRunner.ts`'s `startSideTask` fire-and-forget shape so a caller
   * (the panel's submit button) gets the new id back immediately. */
  start: (params: StartVisualEditParams) => string;
  /** Re-runs the same proposal from scratch — "replayed like normal code
   * changes" per the ROADMAP acceptance criterion. */
  replay: (id: string) => Promise<void>;
  /** Writes `newContent` to `targetFile` via `tool_write_file` (the same
   * permission-gated Rust command a chat turn's `write_file` tool call
   * reaches). On failure the edit stays `pending` with `.error` set so the
   * user can retry; on success it becomes `accepted`. */
  accept: (id: string) => Promise<void>;
  reject: (id: string) => void;
  setAfterScreenshot: (id: string, screenshot: VisualEditScreenshotRef) => void;
  remove: (id: string) => void;
  clear: () => void;
}

function patchEdit(
  state: VisualEditModeState,
  id: string,
  changes: Partial<VisualEdit>,
): Pick<VisualEditModeState, "edits"> {
  const existing = state.edits[id];
  if (!existing) return { edits: state.edits };
  return { edits: { ...state.edits, [id]: { ...existing, ...changes } } };
}

export const useVisualEditModeStore = create<VisualEditModeState>((set, get) => {
  async function runProposal(id: string): Promise<void> {
    const edit = get().edits[id];
    if (!edit) return;
    try {
      const proposal = await proposeVisualEdit({
        element: edit.element,
        description: edit.description,
        pageUrl: edit.pageUrl,
      });
      set((state) =>
        patchEdit(state, id, {
          status: "pending",
          targetFile: proposal.targetFile,
          oldContent: proposal.oldContent,
          newContent: proposal.newContent,
          unifiedDiff: proposal.unifiedDiff,
          summary: proposal.summary,
          error: null,
        }),
      );
    } catch (err) {
      set((state) =>
        patchEdit(state, id, { status: "error", error: errorMessage(err) }),
      );
    }
  }

  return {
    edits: {},
    order: [],

    start: (params) => {
      const id = crypto.randomUUID();
      const edit: VisualEdit = {
        id,
        createdAt: Date.now(),
        sessionId: params.sessionId,
        pageUrl: params.pageUrl,
        description: params.description,
        element: params.element,
        beforeScreenshot: params.beforeScreenshot,
        afterScreenshot: null,
        status: "generating",
        targetFile: null,
        oldContent: null,
        newContent: null,
        unifiedDiff: null,
        summary: null,
        error: null,
      };
      set((state) => ({ edits: { ...state.edits, [id]: edit }, order: [id, ...state.order] }));
      void runProposal(id);
      return id;
    },

    replay: async (id) => {
      const edit = get().edits[id];
      if (!edit) return;
      set((state) =>
        patchEdit(state, id, {
          status: "generating",
          error: null,
          targetFile: null,
          oldContent: null,
          newContent: null,
          unifiedDiff: null,
          summary: null,
        }),
      );
      await runProposal(id);
    },

    accept: async (id) => {
      const edit = get().edits[id];
      if (!edit || edit.status !== "pending" || !edit.targetFile || edit.newContent === null) return;
      try {
        await writeVisualEditToDisk(edit.targetFile, edit.newContent);
        set((state) => patchEdit(state, id, { status: "accepted", error: null }));
      } catch (err) {
        set((state) => patchEdit(state, id, { error: errorMessage(err) }));
        throw err;
      }
    },

    reject: (id) => set((state) => patchEdit(state, id, { status: "rejected" })),

    setAfterScreenshot: (id, screenshot) => set((state) => patchEdit(state, id, { afterScreenshot: screenshot })),

    remove: (id) =>
      set((state) => {
        const edits = { ...state.edits };
        delete edits[id];
        return { edits, order: state.order.filter((existingId) => existingId !== id) };
      }),

    clear: () => set({ edits: {}, order: [] }),
  };
});

export default useVisualEditModeStore;
