import { create } from "zustand";
import type { ArtifactRef } from "../lib/artifacts";

export type ArtifactPaneTab = "preview" | "code";

/** Which artifact `ArtifactPane` currently shows. `sessionId` is carried
 * alongside the ref (unlike `ArtifactBlock.ref` itself, which is
 * session-agnostic — see `artifacts.ts`'s doc comment) because the
 * workspace `<aside>` that hosts `ArtifactPane` is shared by both the main
 * chat pane and the split pane: re-deriving content from
 * `sessionMessages(sessionId)` needs to know which session's transcript to
 * read, and a `messageIndex` alone is ambiguous across two open sessions. */
interface ActiveArtifact {
  sessionId: string;
  ref: ArtifactRef;
}

interface ArtifactStoreState {
  active: ActiveArtifact | null;
  /** Which of ArtifactPane's two tabs is showing. Reset to `'preview'`
   * every time a (possibly different) artifact is opened — the last tab a
   * user picked for one artifact isn't necessarily what they want for the
   * next one they click into. */
  tab: ArtifactPaneTab;
  open: (sessionId: string, ref: ArtifactRef) => void;
  setTab: (tab: ArtifactPaneTab) => void;
  close: () => void;
}

/**
 * Ephemeral per-window UI state for the artifact preview pane — follows
 * `usageStore.ts`'s pure-container pattern (plain `create`, no `persist`
 * middleware, no `invoke` calls of its own). Content is never held here:
 * `ArtifactPane` always re-derives the actual `ArtifactBlock` from
 * `sessionStore`'s messages via `extractArtifacts`/`findArtifact` on every
 * render, so an edit, revert, or compaction that changes the transcript can
 * never leave this pane showing stale content — it either still resolves or
 * the pane shows nothing, never something wrong.
 */
export const useArtifactStore = create<ArtifactStoreState>((set) => ({
  active: null,
  tab: "preview",

  open: (sessionId, ref) => set({ active: { sessionId, ref }, tab: "preview" }),
  setTab: (tab) => set({ tab }),
  close: () => set({ active: null }),
}));

export default useArtifactStore;
