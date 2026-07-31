import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";
import { errorMessage } from "../lib/errors";

/**
 * Background shell commands — the agent's `run_shell` with
 * `run_in_background: true` (see `src-tauri/src/background_shell.rs`).
 *
 * This is the OS-process half of the Background Tasks panel; the other half
 * is model-spawned `task` subagent runs (`subagentStore.ts`). Both are work
 * the app is doing on its own behalf while the user keeps typing, which is
 * exactly what separates them from a SIDE TASK (`sideTaskStore.ts`): a side
 * task is a conversation the user opened deliberately and can talk to in its
 * own pane. Nothing in this store is conversational — a background command
 * has a command line, an output tail, and an exit code.
 *
 * Rust owns the processes; this store is a mirror kept current by the
 * `background-shell-output`/`background-shell-status` events. That means a
 * command started by one window's turn shows up in every window's panel, and
 * a reload re-reads the truth with `refresh()` rather than losing track of a
 * still-running dev server.
 */

export type BackgroundShellStatus = "running" | "exited" | "killed" | "error";

/** Mirrors Rust `BackgroundShellView`. */
export interface BackgroundShellTask {
  id: string;
  command: string;
  cwd: string;
  status: BackgroundShellStatus;
  exit_code: number | null;
  output: string;
  output_truncated: boolean;
  started_at_ms: number;
  finished_at_ms: number | null;
}

interface BackgroundShellOutputEvent {
  id: string;
  chunk: string;
  output_truncated: boolean;
}

interface BackgroundShellStatusEvent {
  task: BackgroundShellTask;
}

/** Frontend retained tail, matching `background_shell.rs`'s own cap so the
 * mirrored output can't outgrow the source of truth. */
export const MAX_BACKGROUND_SHELL_OUTPUT_CHARS = 256 * 1024;

export function appendBoundedShellOutput(current: string, chunk: string): string {
  const combined = current + chunk;
  return combined.length <= MAX_BACKGROUND_SHELL_OUTPUT_CHARS
    ? combined
    : combined.slice(combined.length - MAX_BACKGROUND_SHELL_OUTPUT_CHARS);
}

export function upsertBackgroundShellTask(
  tasks: BackgroundShellTask[],
  next: BackgroundShellTask,
): BackgroundShellTask[] {
  const index = tasks.findIndex((task) => task.id === next.id);
  if (index < 0) return [...tasks, next].sort((a, b) => a.started_at_ms - b.started_at_ms);
  const copy = [...tasks];
  // Keep the locally-streamed output: the status event carries Rust's own
  // snapshot of the tail, which may lag chunks this store already appended.
  copy[index] = { ...next, output: next.output.length >= copy[index].output.length ? next.output : copy[index].output };
  return copy;
}

interface BackgroundShellStore {
  tasks: BackgroundShellTask[];
  initialized: boolean;
  error: string | null;
  /** Attaches the event listeners and loads whatever is already running —
   * safe to call repeatedly (the panel calls it on mount). */
  initialize: () => Promise<void>;
  refresh: () => Promise<void>;
  kill: (id: string) => Promise<void>;
  /** Drops finished entries. Running commands are never touched. */
  clearFinished: () => Promise<void>;
  clearError: () => void;
}

let listenersPromise: Promise<() => void> | null = null;

async function ensureBackgroundShellListeners(): Promise<() => void> {
  if (listenersPromise) return listenersPromise;
  listenersPromise = Promise.all([
    listen<BackgroundShellOutputEvent>("background-shell-output", ({ payload }) => {
      useBackgroundShellStore.setState((state) => ({
        tasks: state.tasks.map((task) =>
          task.id === payload.id
            ? {
                ...task,
                output: appendBoundedShellOutput(task.output, payload.chunk),
                output_truncated: task.output_truncated || payload.output_truncated,
              }
            : task,
        ),
      }));
    }),
    listen<BackgroundShellStatusEvent>("background-shell-status", ({ payload }) => {
      useBackgroundShellStore.setState((state) => ({
        tasks: upsertBackgroundShellTask(state.tasks, payload.task),
      }));
    }),
  ]).then((unlisteners) => () => unlisteners.forEach((unlisten) => unlisten()));
  return listenersPromise;
}

export function disposeBackgroundShellListenersForTests(): void {
  if (listenersPromise) void listenersPromise.then((dispose) => dispose());
  listenersPromise = null;
}

function formatError(error: unknown): string {
  return errorMessage(error);
}

export const useBackgroundShellStore = create<BackgroundShellStore>((set, get) => ({
  tasks: [],
  initialized: false,
  error: null,

  initialize: async () => {
    if (!isTauri()) {
      set({ initialized: true });
      return;
    }
    await ensureBackgroundShellListeners();
    await get().refresh();
    set({ initialized: true });
  },

  refresh: async () => {
    if (!isTauri()) return;
    try {
      const tasks = await invoke<BackgroundShellTask[]>("background_shell_list");
      set({ tasks, error: null });
    } catch (error) {
      set({ error: formatError(error) });
    }
  },

  kill: async (id) => {
    try {
      const task = await invoke<BackgroundShellTask>("background_shell_kill", { id });
      set((state) => ({ tasks: upsertBackgroundShellTask(state.tasks, task), error: null }));
    } catch (error) {
      set({ error: formatError(error) });
    }
  },

  clearFinished: async () => {
    try {
      await invoke("background_shell_clear_finished");
      set((state) => ({ tasks: state.tasks.filter((task) => task.status === "running"), error: null }));
    } catch (error) {
      set({ error: formatError(error) });
    }
  },

  clearError: () => set({ error: null }),
}));

/** Count of still-running commands — feeds the panel badge and the chat's
 * running-work chip, and must agree with the panel's own Running section. */
export function selectRunningShellTaskCount(state: BackgroundShellStore): number {
  return state.tasks.reduce((count, task) => (task.status === "running" ? count + 1 : count), 0);
}

export function selectRunningShellTasks(state: BackgroundShellStore): BackgroundShellTask[] {
  return state.tasks.filter((task) => task.status === "running");
}

export function selectFinishedShellTasks(state: BackgroundShellStore): BackgroundShellTask[] {
  return state.tasks.filter((task) => task.status !== "running");
}
