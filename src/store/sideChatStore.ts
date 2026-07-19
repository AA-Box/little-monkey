import { create } from "zustand";

/**
 * Open/closed state for the floating `/btw` Side Chat panel (SideChatPanel.tsx),
 * keyed per session. The Q&A turns themselves are NOT stored here — they live as
 * `[Btw]` system notices in the session's own messages (see slashCommands.ts /
 * sideQuestion.ts), same as before this panel existed. This store only tracks
 * whether the panel is currently shown, so reopening it (or a fresh `/btw`)
 * replays the session's full side-question history instead of losing it.
 */
interface SideChatState {
  openBySession: Record<string, boolean>;
  open: (sessionId: string) => void;
  close: (sessionId: string) => void;
}

export const useSideChatStore = create<SideChatState>((set) => ({
  openBySession: {},
  open: (sessionId) => set((state) => ({ openBySession: { ...state.openBySession, [sessionId]: true } })),
  close: (sessionId) => set((state) => ({ openBySession: { ...state.openBySession, [sessionId]: false } })),
}));

export const selectSideChatOpen = (sessionId: string) => (state: SideChatState) => state.openBySession[sessionId] ?? false;
