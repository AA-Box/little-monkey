import { create } from "zustand";

export interface BrowserChatEvidence {
  id: string;
  summary: string;
  screenshot: { path: string; dataUrl: string } | null;
}

interface BrowserWorkbenchState {
  pendingBySession: Record<string, BrowserChatEvidence>;
  queueForChat: (sessionId: string, evidence: BrowserChatEvidence) => void;
  consumeForChat: (sessionId: string, evidenceId: string) => void;
}

export const useBrowserWorkbenchStore = create<BrowserWorkbenchState>((set) => ({
  pendingBySession: {},
  queueForChat: (sessionId, evidence) =>
    set((state) => ({
      pendingBySession: { ...state.pendingBySession, [sessionId]: evidence },
    })),
  consumeForChat: (sessionId, evidenceId) =>
    set((state) => {
      if (state.pendingBySession[sessionId]?.id !== evidenceId) return state;
      const pendingBySession = { ...state.pendingBySession };
      delete pendingBySession[sessionId];
      return { pendingBySession };
    }),
}));

export default useBrowserWorkbenchStore;
