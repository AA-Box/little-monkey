import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";

export type TerminalStatus = "running" | "exited" | "killed" | "error";

/** Mirrors Rust `TerminalSessionView` in src-tauri/src/terminal.rs. */
export interface TerminalSession {
  id: string;
  workspace_id: string;
  workspace_path: string;
  shell: string;
  status: TerminalStatus;
  exit_code: number | null;
  output: string;
  output_truncated: boolean;
  started_at_ms: number;
}

interface TerminalOutputEvent {
  session_id: string;
  chunk: string;
  output_truncated: boolean;
}

interface TerminalStatusEvent {
  session: TerminalSession;
}

/** Mirrors Rust `TerminalIdentity` — local `user`/`host` shown in the prompt
 * line. Purely cosmetic; never carries workspace or secret data. */
export interface TerminalIdentity {
  user: string;
  host: string;
}

export interface TerminalEvidence {
  id: string;
  terminalSessionId: string;
  path: string;
  label: string;
  content: string;
  truncated: boolean;
}

export const MAX_TERMINAL_OUTPUT_CHARS = 256 * 1024;
export const MAX_TERMINAL_EVIDENCE_CHARS = 12_000;

/** Removes terminal control sequences and normalizes carriage-return output
 * before search, display, or model-context review. The PTY's raw bounded tail
 * remains in the store so the backend/frontend snapshots stay comparable. */
export function readableTerminalOutput(output: string): string {
  return output
    // CSI plus the short two-byte ESC forms commonly emitted by shells.
    .replace(/\u001B(?:\[[0-?]*[ -/]*[@-~]|[@-_])/g, "")
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n");
}

export function appendBoundedTerminalOutput(current: string, chunk: string): string {
  const combined = current + chunk;
  return combined.length <= MAX_TERMINAL_OUTPUT_CHARS
    ? combined
    : combined.slice(combined.length - MAX_TERMINAL_OUTPUT_CHARS);
}

export function buildTerminalEvidence(
  session: TerminalSession,
  selectedOutput?: string,
  now = Date.now(),
): TerminalEvidence {
  const readable = readableTerminalOutput(selectedOutput?.trim() || session.output).trim();
  const wasTruncated = readable.length > MAX_TERMINAL_EVIDENCE_CHARS || (!selectedOutput && session.output_truncated);
  const content = readable.length > MAX_TERMINAL_EVIDENCE_CHARS
    ? `[Earlier terminal output omitted — evidence is capped at ${MAX_TERMINAL_EVIDENCE_CHARS} characters.]\n${readable.slice(-MAX_TERMINAL_EVIDENCE_CHARS)}`
    : readable;
  const workspaceName = session.workspace_path.split(/[\\/]/).filter(Boolean).pop() ?? session.workspace_path;
  return {
    id: `${session.id}:${now}`,
    terminalSessionId: session.id,
    path: `terminal://${session.id}/${now}.txt`,
    label: `Terminal evidence · ${workspaceName}`,
    content,
    truncated: wasTruncated,
  };
}

interface TerminalStore {
  sessions: TerminalSession[];
  activeSessionId: string | null;
  historyByWorkspace: Record<string, string[]>;
  pendingEvidenceByChat: Record<string, TerminalEvidence[]>;
  identity: TerminalIdentity | null;
  initialized: boolean;
  busy: boolean;
  error: string | null;
  initialize: () => Promise<void>;
  createSession: (workspaceId: string, rows?: number, cols?: number) => Promise<TerminalSession>;
  setActive: (sessionId: string) => void;
  execute: (sessionId: string, command: string) => Promise<void>;
  interrupt: (sessionId: string) => Promise<void>;
  kill: (sessionId: string) => Promise<void>;
  restart: (sessionId: string, rows?: number, cols?: number) => Promise<TerminalSession>;
  close: (sessionId: string) => Promise<void>;
  resize: (sessionId: string, rows: number, cols: number) => Promise<void>;
  loadHistory: (workspaceId: string) => Promise<string[]>;
  loadIdentity: () => Promise<void>;
  queueEvidence: (chatSessionId: string, evidence: TerminalEvidence) => void;
  consumeEvidence: (chatSessionId: string) => TerminalEvidence[];
  clearError: () => void;
}

let listenersPromise: Promise<() => void> | null = null;

function upsertSession(sessions: TerminalSession[], next: TerminalSession): TerminalSession[] {
  const index = sessions.findIndex((session) => session.id === next.id);
  if (index < 0) return [...sessions, next].sort((a, b) => a.started_at_ms - b.started_at_ms);
  const copy = [...sessions];
  copy[index] = next;
  return copy;
}

async function ensureTerminalListeners(): Promise<() => void> {
  if (listenersPromise) return listenersPromise;
  listenersPromise = Promise.all([
    listen<TerminalOutputEvent>("terminal://output", ({ payload }) => {
      useTerminalStore.setState((state) => ({
        sessions: state.sessions.map((session) => session.id === payload.session_id
          ? {
              ...session,
              output: appendBoundedTerminalOutput(session.output, payload.chunk),
              output_truncated: session.output_truncated || payload.output_truncated,
            }
          : session),
      }));
    }),
    listen<TerminalStatusEvent>("terminal://status", ({ payload }) => {
      useTerminalStore.setState((state) => ({ sessions: upsertSession(state.sessions, payload.session) }));
    }),
  ]).then((unlisteners) => () => unlisteners.forEach((unlisten) => unlisten()));
  return listenersPromise;
}

export function disposeTerminalListenersForTests(): void {
  if (listenersPromise) {
    void listenersPromise.then((dispose) => dispose());
  }
  listenersPromise = null;
}

export const useTerminalStore = create<TerminalStore>((set, get) => ({
  sessions: [],
  activeSessionId: null,
  historyByWorkspace: {},
  pendingEvidenceByChat: {},
  identity: null,
  initialized: false,
  busy: false,
  error: null,

  initialize: async () => {
    if (get().initialized) return;
    if (!isTauri()) {
      set({ initialized: true, error: "The integrated terminal is available in the desktop app." });
      return;
    }
    set({ busy: true, error: null });
    try {
      await ensureTerminalListeners();
      const sessions = await invoke<TerminalSession[]>("terminal_list");
      set((state) => ({
        sessions,
        activeSessionId: sessions.some((session) => session.id === state.activeSessionId)
          ? state.activeSessionId
          : sessions[sessions.length - 1]?.id ?? null,
        initialized: true,
        busy: false,
      }));
    } catch (error) {
      set({ initialized: true, busy: false, error: String(error) });
    }
  },

  createSession: async (workspaceId, rows, cols) => {
    set({ busy: true, error: null });
    try {
      const session = await invoke<TerminalSession>("terminal_create", { workspaceId, rows, cols });
      set((state) => ({
        sessions: upsertSession(state.sessions, session),
        activeSessionId: session.id,
        busy: false,
      }));
      await get().loadHistory(workspaceId);
      return session;
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  setActive: (activeSessionId) => set({ activeSessionId }),

  execute: async (sessionId, command) => {
    set({ busy: true, error: null });
    try {
      await invoke("terminal_execute", { sessionId, command });
      const session = get().sessions.find((entry) => entry.id === sessionId);
      if (session) await get().loadHistory(session.workspace_id);
      set({ busy: false });
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  interrupt: async (sessionId) => {
    try {
      await invoke("terminal_interrupt", { sessionId });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  kill: async (sessionId) => {
    try {
      const session = await invoke<TerminalSession>("terminal_kill", { sessionId });
      set((state) => ({ sessions: upsertSession(state.sessions, session) }));
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  restart: async (sessionId, rows, cols) => {
    set({ busy: true, error: null });
    try {
      const session = await invoke<TerminalSession>("terminal_restart", { sessionId, rows, cols });
      set((state) => ({
        sessions: [...state.sessions.filter((entry) => entry.id !== sessionId), session]
          .sort((a, b) => a.started_at_ms - b.started_at_ms),
        activeSessionId: session.id,
        busy: false,
      }));
      return session;
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  close: async (sessionId) => {
    try {
      await invoke("terminal_close", { sessionId });
      set((state) => {
        const sessions = state.sessions.filter((session) => session.id !== sessionId);
        return {
          sessions,
          activeSessionId: state.activeSessionId === sessionId ? sessions[sessions.length - 1]?.id ?? null : state.activeSessionId,
        };
      });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  resize: async (sessionId, rows, cols) => {
    try {
      await invoke("terminal_resize", { sessionId, rows, cols });
    } catch {
      // Resizing is best-effort. A process may exit between ResizeObserver's
      // measurement and the IPC call; that should never obscure its output.
    }
  },

  loadHistory: async (workspaceId) => {
    try {
      const history = await invoke<string[]>("terminal_history", { workspaceId });
      set((state) => ({ historyByWorkspace: { ...state.historyByWorkspace, [workspaceId]: history } }));
      return history;
    } catch (error) {
      set({ error: String(error) });
      return [];
    }
  },

  loadIdentity: async () => {
    if (get().identity || !isTauri()) return;
    try {
      const identity = await invoke<TerminalIdentity>("terminal_identity");
      set({ identity });
    } catch {
      // Cosmetic prompt-line detail only — the panel falls back to the
      // workspace path alone when identity can't be resolved.
    }
  },

  queueEvidence: (chatSessionId, evidence) => set((state) => ({
    pendingEvidenceByChat: {
      ...state.pendingEvidenceByChat,
      [chatSessionId]: [...(state.pendingEvidenceByChat[chatSessionId] ?? []), evidence].slice(-5),
    },
  })),

  consumeEvidence: (chatSessionId) => {
    const evidence = get().pendingEvidenceByChat[chatSessionId] ?? [];
    if (evidence.length > 0) {
      set((state) => {
        const pendingEvidenceByChat = { ...state.pendingEvidenceByChat };
        delete pendingEvidenceByChat[chatSessionId];
        return { pendingEvidenceByChat };
      });
    }
    return evidence;
  },

  clearError: () => set({ error: null }),
}));
