import { create } from "zustand";

import type { ChatMessage } from "../lib/llamaClient";

/**
 * Side Tasks (ROADMAP.md "Phase 1: Core Workspace Parity" -> "Side Tasks"):
 * parallel, interruptible work that is lighter than a daemon run (no
 * cross-window ledger, no remote queue — see `runProtocol.ts`/`runStore.ts`)
 * and more visible than a hidden model-requested `task` subagent call (see
 * `subagentStore.ts`) — a side task is started directly BY THE USER from a
 * piece of context they already have on screen (a chat message, a file
 * selection, terminal output, browser evidence, or an MCP result), runs its
 * own independent model->tools->model loop (`../lib/sideTaskRunner.ts`) in
 * parallel with whatever the main chat is doing, and only ever affects the
 * main chat transcript when the user explicitly clicks "Promote".
 *
 * This store is the single source of truth for every side task's state
 * (queued/running/paused/completed/error/cancelled), its own transcript,
 * tool-call evidence, usage, and produced artifacts, plus the small bits of
 * cross-component UI state (drawer open/closed, selected task, a pending
 * "new side task" composer seed) that let a deeply-nested component like
 * `MessageBubble.tsx` open the drawer with a prefilled composer without
 * threading a callback prop all the way up through `ChatWindow`/`App.tsx` —
 * the same "cross-cutting UI flag lives in a store, not App.tsx local state"
 * pattern `browserWorkbenchStore.ts`'s `pendingBySession` and
 * `artifactStore.ts`'s `active` already establish.
 *
 * Deliberately NOT persisted (no `persist` middleware, unlike
 * `ChatSession.subagentRuns`): a side task is transient, in-session work —
 * closing the app mid-run is the same as hitting Cancel. What DOES survive a
 * side task is only ever what the user explicitly promotes into a chat
 * session (which sessionStore.ts already persists) or into the workspace
 * itself (a `code`-profile task's file writes).
 */

export type SideTaskStatus = "queued" | "running" | "paused" | "completed" | "error" | "cancelled";

/** Tool access offered to a side task's own loop — reuses
 * `tools.ts`'s `toolsForProfile`, the exact same restricted sets already
 * vetted for subagents: `explore` is read-only (read_file/list_dir/glob/
 * grep), `code` adds write_file/edit_file/run_shell through the same
 * permission gate as everything else. Side tasks never offer `task`/`skill`/
 * MCP tools — same depth-1 posture as a subagent (see `tools.ts`'s
 * `toolsForProfile` doc comment). */
export type SideTaskProfile = "explore" | "code";

/** Where a side task's seed prompt came from — every acceptance-listed entry
 * point in ROADMAP.md's "Side Tasks" item gets its own tag so a task's own
 * card can show a small "from chat message" / "from selected files" badge,
 * and so `sideTaskRunner.ts` can build an accurate provenance line into the
 * frozen prompt snapshot. */
export type SideTaskSourceKind =
  | "chat_message"
  | "selected_files"
  | "terminal_output"
  | "browser_evidence"
  | "mcp_result"
  | "manual";

export interface SideTaskSource {
  kind: SideTaskSourceKind;
  /** Short human-readable label, e.g. "Assistant message · 2:14 PM" or
   * "3 selected files". Shown on the task card. */
  label: string;
  /** Bounded preview of the seed context actually captured — NOT necessarily
   * the full `prompt` the model sees (that can be longer); this is what the
   * composer/card show back to the user so they can verify what will be
   * sent before starting the run. */
  excerpt: string;
}

export type SideTaskToolOutcome = "pending" | "succeeded" | "failed" | "denied" | "cancelled";

export interface SideTaskToolEvidence {
  /** The originating `ToolCall.id` — stable identity for updating this same
   * row from "pending" to a terminal outcome once the call finishes. */
  id: string;
  name: string;
  argsPreview: string;
  resultPreview: string;
  outcome: SideTaskToolOutcome;
  startedAt: number;
  finishedAt: number | null;
}

export type SideTaskArtifactKind = "file" | "fence";

export interface SideTaskArtifact {
  id: string;
  kind: SideTaskArtifactKind;
  /** File path for `kind: 'file'`; fence title/kind label for `kind:
   * 'fence'`. */
  label: string;
  preview: string;
}

export interface SideTaskUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export interface SideTaskRecord {
  id: string;
  /** This attempt's own turn id — scopes permission requests/"allow for
   * run" grants and Stop-button cancellation (`tools_cancel_running`) to
   * just this side task, isolated from the main chat's own turn and from
   * every other concurrent side task (see `src-tauri/src/permissions.rs`'s
   * `PermissionState.run_allow`/`turn_mode_overrides` and `tools.rs`'s
   * `tools_cancel_running` — both already keyed per turn id, so a side task
   * gets this isolation for free just by using its own fresh id here rather
   * than the main chat's). A retry gets a BRAND NEW turn id (see `retryOf`)
   * so a stale approval from a previous attempt can never silently apply to
   * the new one. */
  turnId: string;
  /** Id of the side task this one was retried from, or null for an original
   * run — lineage for traceability, never mutated after creation. */
  retryOf: string | null;
  title: string;
  /** The full, frozen instructions sent as this task's one seed user
   * message — captured once at creation and never mutated afterward, so
   * "what prompt actually produced this output" stays answerable even after
   * the user has since changed the source message/selection it was drawn
   * from (traceability acceptance criterion). */
  prompt: string;
  profile: SideTaskProfile;
  status: SideTaskStatus;
  source: SideTaskSource;
  /** The chat session this task was started from — used to resolve the
   * active model at start time and as the default destination for
   * "Promote". */
  sessionId: string;
  /** Human-readable active-model label, frozen at start (mirrors
   * `ResolvedTarget`'s own "resolved once, never re-resolved mid-run"
   * invariant elsewhere in this codebase). */
  modelLabel: string;
  createdAt: number;
  updatedAt: number;
  startedAt: number | null;
  finishedAt: number | null;
  /** This task's own local transcript (seed user message, then
   * assistant/tool messages as its loop progresses) — never written into
   * `sessionStore`'s `messages`, exactly like `subagentStore.ts`'s
   * `liveMessages`. Only ever reaches the main chat transcript via an
   * explicit "Promote" click. */
  messages: ChatMessage[];
  toolEvidence: SideTaskToolEvidence[];
  artifacts: SideTaskArtifact[];
  usage: SideTaskUsage | null;
  error: string | null;
  /** The task's final assistant reply, capped — what "Promote" sends to the
   * main chat. Null until the task reaches a terminal status. */
  finalReport: string | null;
  promotedAt: number | null;
  archivedAt: number | null;
}

export interface SideTaskComposerSeed {
  title: string;
  prompt: string;
  profile: SideTaskProfile;
  source: SideTaskSource;
  sessionId: string;
}

export interface CreateSideTaskParams {
  title: string;
  prompt: string;
  profile: SideTaskProfile;
  source: SideTaskSource;
  sessionId: string;
  modelLabel: string;
  retryOf?: string | null;
}

interface SideTaskStoreState {
  tasks: Record<string, SideTaskRecord>;
  /** Insertion order, newest first — the order `SideTaskDrawer.tsx` lists
   * tasks in. */
  order: string[];
  drawerOpen: boolean;
  selectedTaskId: string | null;
  /** Set by `openComposer` (e.g. a message's "Start side task" action) and
   * consumed once by `SideTaskComposer.tsx`'s prefill effect — mirrors
   * `browserWorkbenchStore.ts`'s `pendingBySession`/`consumeForChat` "staged,
   * not auto-injected" shape, just for opening a form instead of filling a
   * compose box. */
  composerSeed: SideTaskComposerSeed | null;
  composerOpen: boolean;

  openDrawer: () => void;
  closeDrawer: () => void;
  toggleDrawer: () => void;
  selectTask: (id: string | null) => void;
  openComposer: (seed: SideTaskComposerSeed) => void;
  closeComposer: () => void;
  consumeComposerSeed: () => void;

  create: (params: CreateSideTaskParams) => SideTaskRecord;
  markRunning: (id: string) => void;
  appendMessage: (id: string, message: ChatMessage) => void;
  recordToolProposed: (id: string, evidence: SideTaskToolEvidence) => void;
  recordToolFinished: (id: string, toolCallId: string, outcome: SideTaskToolOutcome, resultPreview: string) => void;
  setArtifacts: (id: string, artifacts: SideTaskArtifact[]) => void;
  addUsage: (id: string, usage: SideTaskUsage) => void;
  finish: (id: string, status: "completed" | "error" | "cancelled", finalReport: string | null, error: string | null) => void;
  pause: (id: string) => void;
  resume: (id: string) => void;
  markPromoted: (id: string) => void;
  archive: (id: string) => void;
  unarchive: (id: string) => void;
  remove: (id: string) => void;
}

function patchTask(
  state: SideTaskStoreState,
  id: string,
  patch: Partial<SideTaskRecord> | ((task: SideTaskRecord) => Partial<SideTaskRecord>),
): SideTaskStoreState {
  const existing = state.tasks[id];
  if (!existing) return state;
  const resolved = typeof patch === "function" ? patch(existing) : patch;
  return {
    ...state,
    tasks: { ...state.tasks, [id]: { ...existing, ...resolved, updatedAt: Date.now() } },
  };
}

export const useSideTaskStore = create<SideTaskStoreState>((set, get) => ({
  tasks: {},
  order: [],
  drawerOpen: false,
  selectedTaskId: null,
  composerSeed: null,
  composerOpen: false,

  openDrawer: () => set({ drawerOpen: true }),
  closeDrawer: () => set({ drawerOpen: false }),
  toggleDrawer: () => set((state) => ({ drawerOpen: !state.drawerOpen })),
  selectTask: (id) => set({ selectedTaskId: id }),

  openComposer: (seed) => set({ composerSeed: seed, composerOpen: true, drawerOpen: true }),
  closeComposer: () => set({ composerOpen: false }),
  consumeComposerSeed: () => set({ composerSeed: null }),

  create: (params) => {
    const now = Date.now();
    const record: SideTaskRecord = {
      id: crypto.randomUUID(),
      turnId: crypto.randomUUID(),
      retryOf: params.retryOf ?? null,
      title: params.title.trim() || "Side task",
      prompt: params.prompt,
      profile: params.profile,
      status: "queued",
      source: params.source,
      sessionId: params.sessionId,
      modelLabel: params.modelLabel,
      createdAt: now,
      updatedAt: now,
      startedAt: null,
      finishedAt: null,
      messages: [{ role: "user", content: params.prompt }],
      toolEvidence: [],
      artifacts: [],
      usage: null,
      error: null,
      finalReport: null,
      promotedAt: null,
      archivedAt: null,
    };
    set((state) => ({
      tasks: { ...state.tasks, [record.id]: record },
      order: [record.id, ...state.order],
      selectedTaskId: record.id,
    }));
    return record;
  },

  markRunning: (id) =>
    set((state) => patchTask(state, id, (task) => ({ status: "running", startedAt: task.startedAt ?? Date.now() }))),

  appendMessage: (id, message) =>
    set((state) => patchTask(state, id, (task) => ({ messages: [...task.messages, message] }))),

  recordToolProposed: (id, evidence) =>
    set((state) => patchTask(state, id, (task) => ({ toolEvidence: [...task.toolEvidence, evidence] }))),

  recordToolFinished: (id, toolCallId, outcome, resultPreview) =>
    set((state) =>
      patchTask(state, id, (task) => ({
        toolEvidence: task.toolEvidence.map((entry) =>
          entry.id === toolCallId ? { ...entry, outcome, resultPreview, finishedAt: Date.now() } : entry,
        ),
      })),
    ),

  setArtifacts: (id, artifacts) => set((state) => patchTask(state, id, { artifacts })),

  addUsage: (id, usage) =>
    set((state) =>
      patchTask(state, id, (task) => ({
        usage: task.usage
          ? {
              promptTokens: task.usage.promptTokens + usage.promptTokens,
              completionTokens: task.usage.completionTokens + usage.completionTokens,
              totalTokens: task.usage.totalTokens + usage.totalTokens,
            }
          : usage,
      })),
    ),

  finish: (id, status, finalReport, error) =>
    set((state) => patchTask(state, id, { status, finalReport, error, finishedAt: Date.now() })),

  // Pausing/resuming never touches `startedAt`/`finishedAt` — a paused task
  // is still "the same run", just holding between rounds (see
  // `sideTaskRunner.ts`'s `waitUntilResumed`), not a terminal state.
  pause: (id) =>
    set((state) => {
      const existing = state.tasks[id];
      if (!existing || existing.status !== "running") return state;
      return patchTask(state, id, { status: "paused" });
    }),

  resume: (id) =>
    set((state) => {
      const existing = state.tasks[id];
      if (!existing || existing.status !== "paused") return state;
      return patchTask(state, id, { status: "running" });
    }),

  markPromoted: (id) => set((state) => patchTask(state, id, { promotedAt: Date.now() })),

  archive: (id) => set((state) => patchTask(state, id, { archivedAt: Date.now() })),
  unarchive: (id) => set((state) => patchTask(state, id, { archivedAt: null })),

  remove: (id) =>
    set((state) => {
      if (!state.tasks[id]) return state;
      const tasks = { ...state.tasks };
      delete tasks[id];
      return {
        tasks,
        order: state.order.filter((entry) => entry !== id),
        selectedTaskId: get().selectedTaskId === id ? null : get().selectedTaskId,
      };
    }),
}));

/** Selector: every non-archived task in display order — the drawer's
 * default "Active" list. */
export function selectVisibleSideTasks(state: SideTaskStoreState): SideTaskRecord[] {
  return state.order.map((id) => state.tasks[id]).filter((task): task is SideTaskRecord => Boolean(task) && task.archivedAt === null);
}

/** Selector: every archived task in display order. */
export function selectArchivedSideTasks(state: SideTaskStoreState): SideTaskRecord[] {
  return state.order.map((id) => state.tasks[id]).filter((task): task is SideTaskRecord => Boolean(task) && task.archivedAt !== null);
}

/** Count of tasks that can still produce work — running, waiting to start,
 * or paused (a paused task resumes; it is not finished). Drives the drawer
 * toggle's badge, the drawer header pill, and the chat's "N running tasks"
 * chip, and must agree with the drawer's own "Running" section filter. */
export function selectRunningSideTaskCount(state: SideTaskStoreState): number {
  return state.order.reduce((count, id) => {
    const task = state.tasks[id];
    return task && (task.status === "running" || task.status === "queued" || task.status === "paused") ? count + 1 : count;
  }, 0);
}
