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

/** Where the terminal panel is docked — bottom strip (default) or a
 * right-hand sidebar column. A user-selected UI preference, persisted. */
export type TerminalDock = "bottom" | "right";

const DOCK_STORAGE_KEY = "little-monkey-terminal-dock";
const SIZE_STORAGE_KEY = "little-monkey-terminal-size";

/** Panel size per dock side, px: height when bottom-docked, width when
 * right-docked. Clamped on read so a corrupted stored value can't wedge the
 * panel off-screen. */
export interface TerminalPanelSize {
  bottom: number;
  right: number;
}

export const TERMINAL_MIN_SIZE = 180;
const DEFAULT_SIZE: TerminalPanelSize = { bottom: 320, right: 420 };

export function clampTerminalSize(value: number): number {
  const viewportCap = Math.max(
    TERMINAL_MIN_SIZE,
    Math.floor((typeof window === "undefined" ? 1200 : Math.max(window.innerWidth, window.innerHeight)) * 0.85),
  );
  return Math.min(viewportCap, Math.max(TERMINAL_MIN_SIZE, Math.round(value)));
}

function readInitialDock(): TerminalDock {
  try {
    const stored = localStorage.getItem(DOCK_STORAGE_KEY);
    return stored === "right" ? "right" : "bottom";
  } catch {
    return "bottom";
  }
}

function readInitialSize(): TerminalPanelSize {
  try {
    const stored = JSON.parse(localStorage.getItem(SIZE_STORAGE_KEY) ?? "");
    return {
      bottom: clampTerminalSize(Number(stored?.bottom) || DEFAULT_SIZE.bottom),
      right: clampTerminalSize(Number(stored?.right) || DEFAULT_SIZE.right),
    };
  } catch {
    return { ...DEFAULT_SIZE };
  }
}

/** Removes terminal control sequences and normalizes carriage-return output
 * before search, display, or model-context review. The PTY's raw bounded tail
 * remains in the store so the backend/frontend snapshots stay comparable. */
export function readableTerminalOutput(output: string): string {
  return output
    // String-payload sequences first — DCS/SOS/PM/APC and OSC (window title,
    // cwd, shell-integration markers) run until BEL or ST, and their payload
    // can contain anything.
    .replace(/\u001B[P^_X][\s\S]*?(?:\u0007|\u001B\\)/g, "")
    .replace(/\u001B\][\s\S]*?(?:\u0007|\u001B\\)/g, "")
    // CSI (colors, cursor moves, bracketed-paste mode).
    .replace(/\u001B\[[0-?]*[ -/]*[@-~]/g, "")
    // Charset designation: ESC ( ) * + plus one selector character.
    .replace(/\u001B[()*+][\s\S]/g, "")
    // Every remaining ESC-plus-one-character form. This is the one that was
    // missing: a zsh prompt emits keypad-mode `ESC =` / `ESC >` and `ESC c`
    // around each redraw, and matching only the `ESC @` through `ESC _`
    // range left the `=`, `>` and `c` behind as literal text in the middle
    // of the command echo ("ccat ~/Library/Logs/bf6.log>").
    .replace(/\u001B[ -~]/g, "")
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    // Whatever is still left: the C0 controls prompt frameworks wrap
    // non-printing runs in (zsh's %{…%} becomes SOH/STX), DEL, and any stray
    // escape from a sequence split across two PTY chunks. They render as
    // boxes in a plain <pre>. Newline and tab are the only C0 characters
    // this text keeps.
    .replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, "");
}

/** Lines of `script` a PTY must receive as separate submissions.
 * `terminal_execute` takes exactly one line (see `terminal.rs`), which is also
 * what typing a multi-line snippet by hand does — an interactive shell handles
 * `\` continuations and heredoc bodies itself as the lines arrive. Blank lines
 * are dropped: the Rust side rejects an empty submission, and a blank line at a
 * prompt is a no-op anyway. */
export function terminalCommandLines(script: string): string[] {
  return script.split("\n").map((line) => line.trim()).filter((line) => line.length > 0);
}

/** `text` wrapped in the terminal's bracketed-paste markers when it spans more
 * than one line, so the shell puts every line into its edit buffer instead of
 * running all but the last. zsh and bash enable bracketed paste by default; a
 * shell that doesn't treats the markers as no-op escape sequences, which is
 * exactly the behaviour a plain write already had. */
export function bracketedPasteInput(text: string): string {
  return text.includes("\n") ? `\u001B[200~${text}\u001B[201~` : text;
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
  /** User-selected dock side for the panel, persisted across restarts. */
  dock: TerminalDock;
  /** User-dragged panel size per dock side (px), persisted across restarts. */
  panelSize: TerminalPanelSize;
  setDock: (dock: TerminalDock) => void;
  setPanelSize: (dock: TerminalDock, size: number) => void;
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
  /** Submits a possibly multi-line snippet one line at a time, in order —
   * `execute` itself only accepts a single line. Stops at the first line that
   * fails so a broken step cannot be followed by the rest of the script. */
  executeScript: (sessionId: string, script: string) => Promise<void>;
  /** A command handed to the terminal panel from elsewhere in the app — a
   * chat code block's "Open in terminal". Typed into the active session's
   * shell and deliberately NOT submitted, so the user still reads it and
   * presses Enter themselves. */
  pendingCommand: string | null;
  /** Newest running session for `workspaceId`, started if there isn't one.
   * Both chat code-block actions go through this so repeated clicks reuse one
   * shell instead of spawning a PTY per click. */
  ensureSession: (workspaceId: string) => Promise<TerminalSession>;
  requestCommand: (command: string) => void;
  /** Reads the pending command and clears it in one step, returning null when
   * there was none. Atomic on purpose: the panel consumes this from an
   * effect, and React StrictMode runs that effect twice in development — a
   * separate read-then-clear typed the command into the shell twice. */
  consumePendingCommand: () => string | null;
  /** Raw keystroke path for the embedded emulator: forwards bytes to the PTY
   * verbatim (arrows, tab, ^C, pastes) so the user's real shell does its own
   * line editing/history/completions. Fire-and-forget error surface — a
   * failed keystroke sets the store error but never throws mid-typing. */
  write: (sessionId: string, data: string) => Promise<void>;
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
const terminalWriteQueues = new Map<string, Promise<unknown>>();

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
  terminalWriteQueues.clear();
}

export const useTerminalStore = create<TerminalStore>((set, get) => ({
  sessions: [],
  activeSessionId: null,
  dock: readInitialDock(),
  panelSize: readInitialSize(),

  setDock: (dock) => {
    set({ dock });
    try {
      localStorage.setItem(DOCK_STORAGE_KEY, dock);
    } catch {
      // Best-effort persistence only.
    }
  },

  setPanelSize: (dock, size) => {
    const clamped = clampTerminalSize(size);
    set((state) => {
      const panelSize = { ...state.panelSize, [dock]: clamped };
      try {
        localStorage.setItem(SIZE_STORAGE_KEY, JSON.stringify(panelSize));
      } catch {
        // Best-effort persistence only.
      }
      return { panelSize };
    });
  },
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
      await terminalWriteQueues.get(sessionId)?.catch(() => undefined);
      await invoke("terminal_execute", { sessionId, command });
      const session = get().sessions.find((entry) => entry.id === sessionId);
      if (session) await get().loadHistory(session.workspace_id);
      set({ busy: false });
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  executeScript: async (sessionId, script) => {
    for (const line of terminalCommandLines(script)) {
      await get().execute(sessionId, line);
    }
  },

  pendingCommand: null,

  ensureSession: async (workspaceId) => {
    await get().initialize();
    const running = get().sessions.filter(
      (session) => session.workspace_id === workspaceId && session.status === "running",
    );
    // Sessions are kept in start order by `upsertSession`, so the last running
    // one is the newest — the same one the panel shows by default.
    return running[running.length - 1] ?? get().createSession(workspaceId);
  },

  requestCommand: (command) => set({ pendingCommand: command }),

  consumePendingCommand: () => {
    const command = get().pendingCommand;
    if (command !== null) set({ pendingCommand: null });
    return command;
  },

  write: async (sessionId, data) => {
    const previous = terminalWriteQueues.get(sessionId) ?? Promise.resolve();
    const pending = previous
      .catch(() => undefined)
      .then(() => invoke("terminal_write", { sessionId, data }));
    terminalWriteQueues.set(sessionId, pending);
    try {
      await pending;
      // The backend, not this raw byte stream, decides whether a submitted
      // line was reconstructable and safe enough for app-owned history.
      // Reload only after an Enter-shaped write so ordinary typing stays
      // latency-free and shell editing/Ctrl+C remain entirely native.
      if (data.includes("\r") || data.includes("\n")) {
        const session = get().sessions.find((entry) => entry.id === sessionId);
        if (session) await get().loadHistory(session.workspace_id);
      }
    } catch (error) {
      set({ error: String(error) });
    } finally {
      if (terminalWriteQueues.get(sessionId) === pending) {
        terminalWriteQueues.delete(sessionId);
      }
    }
  },

  interrupt: async (sessionId) => {
    try {
      await terminalWriteQueues.get(sessionId)?.catch(() => undefined);
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
