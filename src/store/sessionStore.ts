import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { textContent, type ChatMessage } from "../lib/llamaClient";
import { primaryRoot, useWorkspaceStore } from "./workspaceStore";
import { usePromptStore } from "./promptStore";

/** localStorage key sessions were persisted under BEFORE file-based
 * persistence existed — only read (once, then removed) to migrate old data.
 * See `hydrateSessions`. */
const LEGACY_STORAGE_KEY = "little-monkey-chat-sessions";

/** Emitted by the backend after every successful `sessions_save`, with the
 * saving window's label as payload (see src-tauri/src/sessions.rs). Other
 * windows rehydrate from the file on it so two open windows stop clobbering
 * each other's chats. */
const SESSIONS_CHANGED_EVENT = "sessions://changed";

/** How long after the last mutation the debounced file write fires. Streaming
 * mutates the last message on every delta — persisting each one would issue
 * an IPC file write per token, so writes are coalesced instead. */
const PERSIST_DEBOUNCE_MS = 400;

/** Default title assigned to a freshly created session, before its first
 * user message (if any) derives a real one. */
const DEFAULT_TITLE = "New session";

/** Max length (in characters, before the ellipsis) a title derived from a
 * user message is truncated to. */
const TITLE_MAX_LENGTH = 48;

/** A single saved conversation: its own message transcript plus metadata
 * used to list/sort/label it in the session sidebar. */
export interface ChatSession {
  id: string;
  title: string;
  messages: ChatMessage[];
  createdAt: number;
  updatedAt: number;
  /** Shown pinned to the top of the sidebar, above "Recents". */
  pinned: boolean;
  /** Manually toggled via the session menu; cleared automatically the next
   * time the session becomes active (see `switchSession`). */
  unread: boolean;
  /** Hidden from the main sidebar (Pinned/groups/Recents) once archived;
   * only reachable via the collapsed "Archived" footer section. */
  archived: boolean;
  /** id of a `SessionGroup`, or null if not grouped (shown under
   * "Recents"). */
  groupId: string | null;
  /** Snapshot of the primary workspace root's path at creation time, used
   * by the session menu's "Open in" actions (Finder/terminal/editor) —
   * sessions aren't otherwise tied to a workspace, since the workspace is
   * app-global and can change after a session is created. Falls back to
   * the current primary root at click time if null (older sessions). */
  workspacePath: string | null;
  /** id of a `PromptEntry` (`kind: "persona"`, see `promptStore.ts`) applied
   * on top of the base system prompt for this session, or `null` for none —
   * set via `setSessionPersona` (the `PersonaSelector` toolbar pill, or
   * picking a persona row in the "/"-command popup). Per-session rather than
   * app-global so the split pane can run two different personas side by
   * side. Looked up fresh every turn (see `agentLoop.ts`/`systemPrompt.ts`'s
   * `resolvePersona`), so a deleted persona resolves to null instead of
   * leaving a session broken. */
  personaId: string | null;
}

/** A user-defined grouping sessions can be filed under via the session
 * menu's "Move to group" — purely a label, no other behavior attached. */
export interface SessionGroup {
  id: string;
  name: string;
}

/**
 * Chat session state for the workspace.
 *
 * This store manages multiple independent conversations (`sessions`). The
 * active transcript is mirrored as `messages` for ergonomic subscriptions,
 * while each session still owns its own persisted transcript. Message
 * mutations take an explicit session id — the primary pane (bound to
 * `activeSessionId`) and the split pane (bound to `splitSessionId`, see
 * App.tsx) can stream turns into different sessions concurrently.
 *
 * Persistence lives in a file in the app data directory (see
 * src-tauri/src/sessions.rs), written debounced after every mutation —
 * localStorage's ~5MB quota made inline base64 image attachments a silent
 * data-loss hazard. Call `hydrateSessions()` once at startup (main.tsx does)
 * before the first render.
 */
export interface SessionStore {
  /** All saved sessions, in no particular order (sort at render time). */
  sessions: ChatSession[];
  /** User-defined groups sessions can be filed under, in creation order. */
  groups: SessionGroup[];
  /** Messages for `activeSessionId`, mirrored for ergonomic UI subscriptions. */
  messages: ChatMessage[];
  /** id of the session shown in the primary chat pane (what the sidebar
   * highlights and switches). */
  activeSessionId: string;
  /** id of the session shown in the split pane, or null when no split is
   * open. Per-window UI state — never persisted. */
  splitSessionId: string | null;
  /** Session ids with an agent turn currently in flight (either pane, or a
   * turn orphaned by a pane switch that is still streaming). Keyed by
   * session — NOT by pane — so a pane landing on a session mid-turn shows
   * it as busy and can stop it. Never persisted. */
  runningTurns: Record<string, true>;
  /** Session ids currently executing a verification command, mapped to that
   * command's label — see `setRunningVerifyLabel`. Never persisted. */
  runningVerifyLabel: Record<string, string>;
  /** Last file-persistence failure, surfaced in the UI (ChatWindow banner)
   * instead of silently dropping history; cleared by the next successful
   * save. */
  persistError: string | null;
  /** Create a fresh empty session and make it active. */
  newSession: () => void;
  /** Make `id` the active session (no-op if it doesn't exist). Clears the
   * target's `unread` flag, mirroring "opening it marks it read". */
  switchSession: (id: string) => void;
  /** Remove a session; if it was active, activate the next-most-recent one
   * (or a brand new session if none remain). */
  deleteSession: (id: string) => void;
  /** Rename a session's title (trimmed; no-op if the result is empty). */
  renameSession: (id: string, title: string) => void;
  /** Toggle whether a session is pinned to the top of the sidebar. */
  togglePin: (id: string) => void;
  /** Toggle a session's unread flag. */
  toggleUnread: (id: string) => void;
  /** Hide a session from the main sidebar sections. Also un-pins it. */
  archiveSession: (id: string) => void;
  /** Restore an archived session to the main sidebar sections. */
  unarchiveSession: (id: string) => void;
  /** Duplicate a session (title, messages, group, workspace path) as a new
   * session and switch to it. */
  forkSession: (id: string) => void;
  /** Create a new group and return its id (empty string, no-op if `name`
   * is blank after trimming). */
  createGroup: (name: string) => string;
  /** File a session under a group, or clear its group with `null`. */
  moveToGroup: (sessionId: string, groupId: string | null) => void;
  /** Sets (or clears with `null`) the persona applied to `sessionId`'s system
   * prompt — see `ChatSession.personaId`. */
  setSessionPersona: (sessionId: string, personaId: string | null) => void;
  /** Record whether an agent turn is in flight for `sessionId` — called
   * only by `runAgentTurn` (start/finally). */
  markTurnRunning: (sessionId: string, running: boolean) => void;
  /** Sets (or clears with `null`) the label of the verification command
   * currently executing for `sessionId` — called only by
   * `runVerificationPhase` (agentLoop.ts), around each `verify_run` invoke,
   * so `MessageList` can show a "running <label>…" indicator for
   * potentially long test suites (see the design doc's "long-running test
   * suites stall the turn" risk). Mirrors `markTurnRunning`'s shape. */
  setRunningVerifyLabel: (sessionId: string, label: string | null) => void;
  /** Open `id` in the split pane (no-op if it doesn't exist). Clears the
   * target's `unread` flag, mirroring "opening it marks it read". */
  openSplit: (id: string) => void;
  /** Close the split pane. */
  closeSplit: () => void;

  /** Append a new message (user, assistant, or tool) to `sessionId`'s
   * transcript. No-ops if the session no longer exists (deleted while a
   * turn was still streaming into it) — as do all mutators below. */
  addMessage: (sessionId: string, msg: ChatMessage) => void;
  /**
   * Shallow-merge `patch` into the last message of `sessionId`'s
   * transcript. Used by the agent loop to stream assistant content/tool_calls
   * into the in-progress message as chunks arrive. No-ops if the session
   * has no messages.
   */
  updateLastMessage: (sessionId: string, patch: Partial<ChatMessage>) => void;
  /** Shallow-merge `patch` into the message at `index` of `sessionId`'s
   * transcript. Used by the checkpoint notice's Revert button to
   * mark itself reverted in place. No-ops if `index` is out of range. */
  updateMessageAt: (sessionId: string, index: number, patch: Partial<ChatMessage>) => void;
  /** Drops the last message of `sessionId`'s transcript. Used to
   * clean up an empty assistant placeholder left behind when the Stop
   * button cancels a turn before any content streamed in. No-ops if the
   * session has no messages. */
  removeLastMessage: (sessionId: string) => void;
  /** Keeps only messages[0, index) of `sessionId`'s transcript,
   * discarding the message at `index` and everything after it. Used when
   * editing a past user message: drop it and its whole downstream reply
   * before resubmitting the edited text as a fresh turn. */
  truncateFromIndex: (sessionId: string, index: number) => void;
  /** Clear `sessionId`'s transcript back to empty. */
  clear: (sessionId: string) => void;
  /** Replaces `sessionId`'s entire transcript with `messages` — used by the context-trimmer to persist a compaction (dropped/summarized messages replaced with a visible marker) so it isn't redone every turn. */
  replaceMessages: (sessionId: string, messages: ChatMessage[]) => void;
}

/** Stable empty transcript so `selectSessionMessages` never returns a fresh
 * array for a missing session (which would re-render subscribers forever). */
const EMPTY_MESSAGES: ChatMessage[] = [];

/** Zustand selector for one session's transcript, for the chat panes:
 * `useSessionStore(selectSessionMessages(sessionId))`. */
export function selectSessionMessages(sessionId: string) {
  return (state: SessionStore): ChatMessage[] =>
    state.sessions.find((s) => s.id === sessionId)?.messages ?? EMPTY_MESSAGES;
}

/** Non-reactive read of one session's transcript (agent loop, event
 * handlers). */
export function sessionMessages(sessionId: string): ChatMessage[] {
  return selectSessionMessages(sessionId)(useSessionStore.getState());
}

/** Zustand selector: whether an agent turn is in flight for `sessionId`. */
export function selectTurnRunning(sessionId: string) {
  return (state: SessionStore): boolean => state.runningTurns[sessionId] === true;
}

/** Zustand selector: the label of the verification command currently
 * executing for `sessionId`, or `null` when none is running. */
export function selectRunningVerifyLabel(sessionId: string) {
  return (state: SessionStore): string | null => state.runningVerifyLabel[sessionId] ?? null;
}

function createSession(): ChatSession {
  const now = Date.now();
  return {
    id: crypto.randomUUID(),
    title: DEFAULT_TITLE,
    messages: [],
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    workspacePath: primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null,
    // New sessions start on the user's chosen default persona, if any (see
    // `promptStore.ts`'s `defaultPersonaId` / `setDefaultPersona`). A dangling
    // id (its persona got deleted) resolves to "None" at turn time same as
    // any other persona reference — see `composeSystemPrompt`.
    personaId: usePromptStore.getState().defaultPersonaId,
  };
}

/** Builds the new session a "Fork" action produces: same messages/group/
 * workspace path/persona as `source`, fresh id/title/timestamps, and reset
 * pinned/unread/archived flags. */
function cloneSessionAsFork(source: ChatSession): ChatSession {
  const now = Date.now();
  return {
    id: crypto.randomUUID(),
    title: `${source.title} (fork)`,
    messages: source.messages.map((m) => ({ ...m })),
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: source.groupId,
    workspacePath: source.workspacePath,
    personaId: source.personaId,
  };
}

/** Trims whitespace, collapses internal newlines to spaces, and truncates to
 * `TITLE_MAX_LENGTH` characters (appending an ellipsis if it was longer). */
function deriveTitle(content: string): string {
  const collapsed = content.trim().replace(/\n+/g, " ");
  if (collapsed.length <= TITLE_MAX_LENGTH) return collapsed;
  return `${collapsed.slice(0, TITLE_MAX_LENGTH)}…`;
}

interface PersistedShape {
  sessions: ChatSession[];
  activeSessionId: string;
  groups: SessionGroup[];
}

/** Fills in defaults for fields added after a session may have already been
 * persisted, so older persisted data hydrates cleanly. */
function normalizeMessage(raw: unknown): ChatMessage | null {
  if (typeof raw === "string") {
    return { role: "user", content: raw };
  }
  if (!raw || typeof raw !== "object") return null;
  const candidate = raw as Partial<ChatMessage>;
  if (
    (candidate.role === "system" || candidate.role === "user" || candidate.role === "assistant" || candidate.role === "tool") &&
    (typeof candidate.content === "string" || Array.isArray(candidate.content))
  ) {
    return candidate as ChatMessage;
  }
  return null;
}

function normalizeSession(raw: Partial<ChatSession>): ChatSession {
  const messages = Array.isArray(raw.messages)
    ? raw.messages.map(normalizeMessage).filter((message): message is ChatMessage => message !== null)
    : [];
  return {
    id: raw.id as string,
    title: raw.title as string,
    messages,
    createdAt: raw.createdAt as number,
    updatedAt: raw.updatedAt as number,
    pinned: raw.pinned ?? false,
    unread: raw.unread ?? false,
    archived: raw.archived ?? false,
    groupId: raw.groupId ?? null,
    workspacePath: raw.workspacePath ?? null,
    personaId: typeof raw.personaId === "string" ? raw.personaId : null,
  };
}

/** Parses and validates a persisted `{ sessions, activeSessionId, groups }`
 * JSON blob (from the sessions file, or legacy localStorage). Returns `null`
 * for anything absent, corrupt, or malformed. */
function parsePersisted(raw: string | null): PersistedShape | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (
      !parsed ||
      !Array.isArray(parsed.sessions) ||
      parsed.sessions.length === 0 ||
      typeof parsed.activeSessionId !== "string"
    ) {
      return null;
    }
    return {
      sessions: parsed.sessions.map(normalizeSession),
      activeSessionId: parsed.activeSessionId,
      groups: Array.isArray(parsed.groups) ? parsed.groups : [],
    };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Debounced file persistence.
//
// Every store mutation calls `persist(...)`, which stashes the latest
// snapshot and (re)uses a single trailing timer — a streaming turn's
// per-token `updateLastMessage` calls coalesce into one file write per
// PERSIST_DEBOUNCE_MS. `flushPersist` always writes the LATEST snapshot, so
// nothing is ever lost to coalescing, only delayed.
// ---------------------------------------------------------------------------

let persistTimer: ReturnType<typeof setTimeout> | null = null;
let pendingPayload: string | null = null;

function flushPersist(): void {
  if (persistTimer !== null) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  const payload = pendingPayload;
  pendingPayload = null;
  if (payload === null) return;

  invoke("sessions_save", { payload })
    .then(() => {
      if (useSessionStore.getState().persistError !== null) {
        useSessionStore.setState({ persistError: null });
      }
    })
    .catch((err: unknown) => {
      useSessionStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    });
}

function persist(sessions: ChatSession[], activeSessionId: string, groups: SessionGroup[]): void {
  try {
    pendingPayload = JSON.stringify({ sessions, activeSessionId, groups });
  } catch (err) {
    useSessionStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    return;
  }
  if (persistTimer === null) {
    persistTimer = setTimeout(flushPersist, PERSIST_DEBOUNCE_MS);
  }
}

// Best-effort flush of a pending (debounced) write when the window goes away
// mid-debounce. The IPC message is posted synchronously even though its
// completion can't be awaited here.
if (typeof window !== "undefined") {
  window.addEventListener("beforeunload", flushPersist);
}

/**
 * Re-reads the saved blob after ANOTHER window persisted it, replacing this
 * window's sessions/groups wholesale. This window's own `activeSessionId` is
 * preserved (rehydration must not yank the user to the other window's active
 * session), falling back to the file's only if ours was deleted over there.
 * Read errors are ignored: the current in-memory state stays, and this
 * window's own next save will surface any real persistence problem.
 */
async function rehydrateFromFile(): Promise<void> {
  let fromFile: PersistedShape | null = null;
  try {
    const raw = await invoke<string | null>("sessions_load");
    fromFile = parsePersisted(raw);
  } catch {
    return;
  }
  if (!fromFile) return;

  const { activeSessionId: localActiveId, splitSessionId: localSplitId } = useSessionStore.getState();
  const activeSessionId = fromFile.sessions.some((s) => s.id === localActiveId)
    ? localActiveId
    : fromFile.activeSessionId;
  // The split pane is per-window state, but its session may have been
  // deleted in the other window — close the pane rather than point it at a
  // session that no longer exists. And if the primary pane just fell back
  // to the file's active session, that may be the split session — close the
  // pane rather than show one transcript in both panes (see `openSplit`).
  const splitSessionId =
    localSplitId !== null && localSplitId !== activeSessionId && fromFile.sessions.some((s) => s.id === localSplitId)
      ? localSplitId
      : null;

  useSessionStore.setState({
    sessions: fromFile.sessions,
    groups: fromFile.groups,
    activeSessionId,
    splitSessionId,
    messages: messagesOf(fromFile.sessions, activeSessionId),
  });
}

/** Starts listening for other windows' saves (see `SESSIONS_CHANGED_EVENT`).
 * Called once per window from `hydrateSessions`. */
async function listenForOtherWindowSaves(): Promise<void> {
  const ownLabel = getCurrentWindow().label;
  await listen<string>(SESSIONS_CHANGED_EVENT, (event) => {
    // Our own save — the store already reflects it.
    if (event.payload === ownLabel) return;
    // A local mutation is still waiting in the debounce window: rehydrating
    // now would visibly discard it (e.g. a just-typed user message). Skip —
    // our imminent flush notifies the other window instead, and subsequent
    // events converge us onto whoever saved last.
    if (pendingPayload !== null) return;
    void rehydrateFromFile();
  });
}

/**
 * Loads persisted sessions from the app-data file (see
 * src-tauri/src/sessions.rs) into the store, falling back to — and migrating
 * from — the pre-file-persistence localStorage blob the first time. Must be
 * awaited before the first render (main.tsx does) so a user action can never
 * race the hydrate and get overwritten by it. Also subscribes this window to
 * other windows' saves so multi-window use stays in sync.
 */
export async function hydrateSessions(): Promise<void> {
  // Subscribe before the initial load so a save landing in another window
  // during hydration isn't missed.
  void listenForOtherWindowSaves().catch((err: unknown) => {
    console.error("Failed to subscribe to cross-window session sync", err);
  });
  let fromFile: PersistedShape | null = null;
  try {
    const raw = await invoke<string | null>("sessions_load");
    fromFile = parsePersisted(raw);
  } catch (err) {
    // Read failure (not "file missing" — that returns null). Keep the fresh
    // in-memory session and surface the error; the file on disk is left
    // untouched until the user actually does something worth saving.
    useSessionStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    return;
  }

  if (fromFile) {
    useSessionStore.setState({
      sessions: fromFile.sessions,
      groups: fromFile.groups,
      activeSessionId: fromFile.activeSessionId,
      messages: messagesOf(fromFile.sessions, fromFile.activeSessionId),
    });
    return;
  }

  // No sessions file yet — migrate the legacy localStorage blob if present.
  let legacyRaw: string | null = null;
  try {
    legacyRaw = localStorage.getItem(LEGACY_STORAGE_KEY);
  } catch {
    // localStorage unavailable — nothing to migrate.
  }
  const legacy = parsePersisted(legacyRaw);
  if (!legacy) return; // keep the fresh initial session

  useSessionStore.setState({
    sessions: legacy.sessions,
    groups: legacy.groups,
    activeSessionId: legacy.activeSessionId,
    messages: messagesOf(legacy.sessions, legacy.activeSessionId),
  });

  try {
    await invoke("sessions_save", {
      payload: JSON.stringify({ sessions: legacy.sessions, activeSessionId: legacy.activeSessionId, groups: legacy.groups }),
    });
    // Only drop the legacy copy once the file write actually succeeded.
    localStorage.removeItem(LEGACY_STORAGE_KEY);
  } catch (err) {
    useSessionStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
  }
}

function messagesOf(sessions: ChatSession[], activeSessionId: string): ChatMessage[] {
  return sessions.find((s) => s.id === activeSessionId)?.messages ?? [];
}

const initialSession = createSession();

export const useSessionStore = create<SessionStore>((set, get) => ({
  sessions: [initialSession],
  groups: [],
  activeSessionId: initialSession.id,
  splitSessionId: null,
  messages: initialSession.messages,
  runningTurns: {},
  runningVerifyLabel: {},
  persistError: null,

  markTurnRunning: (sessionId, running) => {
    set((state) => {
      if (running) return { runningTurns: { ...state.runningTurns, [sessionId]: true } };
      if (!(sessionId in state.runningTurns)) return state;
      const runningTurns = { ...state.runningTurns };
      delete runningTurns[sessionId];
      return { runningTurns };
    });
  },

  setRunningVerifyLabel: (sessionId, label) => {
    set((state) => {
      if (label !== null) return { runningVerifyLabel: { ...state.runningVerifyLabel, [sessionId]: label } };
      if (!(sessionId in state.runningVerifyLabel)) return state;
      const runningVerifyLabel = { ...state.runningVerifyLabel };
      delete runningVerifyLabel[sessionId];
      return { runningVerifyLabel };
    });
  },

  newSession: () => {
    const session = createSession();
    set((state) => {
      const sessions = [...state.sessions, session];
      persist(sessions, session.id, state.groups);
      return { sessions, activeSessionId: session.id, messages: session.messages };
    });
  },

  switchSession: (id) => {
    const state = get();
    const target = state.sessions.find((s) => s.id === id);
    if (!target) return;
    const sessions = target.unread
      ? state.sessions.map((s) => (s.id === id ? { ...s, unread: false } : s))
      : state.sessions;
    persist(sessions, id, state.groups);
    // Switching the primary pane onto the session the split pane is showing
    // would put one transcript in both panes (see `openSplit`) — the split
    // pane closes instead, as if its content moved to the primary pane.
    const splitSessionId = state.splitSessionId === id ? null : state.splitSessionId;
    set({ sessions, activeSessionId: id, splitSessionId, messages: sessions.find((s) => s.id === id)!.messages });
  },

  deleteSession: (id) => {
    set((state) => {
      const remaining = state.sessions.filter((s) => s.id !== id);
      // Close the split pane if it was showing the deleted session.
      const splitSessionId = state.splitSessionId === id ? null : state.splitSessionId;

      if (state.activeSessionId !== id) {
        persist(remaining, state.activeSessionId, state.groups);
        return { sessions: remaining, splitSessionId };
      }

      let nextActive: ChatSession;
      let sessions: ChatSession[];
      if (remaining.length === 0) {
        nextActive = createSession();
        sessions = [nextActive];
      } else {
        nextActive = remaining.reduce((mostRecent, session) =>
          session.updatedAt > mostRecent.updatedAt ? session : mostRecent
        );
        sessions = remaining;
      }

      persist(sessions, nextActive.id, state.groups);
      // The promoted session may be the one the split pane is showing —
      // close the pane rather than show one transcript twice (see
      // `openSplit`).
      return {
        sessions,
        activeSessionId: nextActive.id,
        splitSessionId: splitSessionId === nextActive.id ? null : splitSessionId,
        messages: nextActive.messages,
      };
    });
  },

  renameSession: (id, title) => {
    const trimmed = title.trim();
    if (!trimmed) return;
    set((state) => {
      const sessions = state.sessions.map((s) =>
        s.id === id ? { ...s, title: trimmed, updatedAt: Date.now() } : s
      );
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  togglePin: (id) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === id ? { ...s, pinned: !s.pinned } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  toggleUnread: (id) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === id ? { ...s, unread: !s.unread } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  archiveSession: (id) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === id ? { ...s, archived: true, pinned: false } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  unarchiveSession: (id) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === id ? { ...s, archived: false } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  forkSession: (id) => {
    set((state) => {
      const source = state.sessions.find((s) => s.id === id);
      if (!source) return state;
      const fork = cloneSessionAsFork(source);
      const sessions = [...state.sessions, fork];
      persist(sessions, fork.id, state.groups);
      return { sessions, activeSessionId: fork.id, messages: fork.messages };
    });
  },

  createGroup: (name) => {
    const trimmed = name.trim();
    if (!trimmed) return "";
    const id = crypto.randomUUID();
    set((state) => {
      const groups = [...state.groups, { id, name: trimmed }];
      persist(state.sessions, state.activeSessionId, groups);
      return { groups };
    });
    return id;
  },

  moveToGroup: (sessionId, groupId) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === sessionId ? { ...s, groupId } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  setSessionPersona: (sessionId, personaId) => {
    set((state) => {
      const sessions = state.sessions.map((s) => (s.id === sessionId ? { ...s, personaId } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions };
    });
  },

  openSplit: (id) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === id);
      if (!target) return state;
      // Never show the same session in both panes: two ChatWindows would
      // each run turns into one transcript concurrently, interleaving their
      // streamed updates. Opening the active session is a silent no-op.
      if (id === state.activeSessionId) return state;
      if (!target.unread) return { splitSessionId: id };
      const sessions = state.sessions.map((s) => (s.id === id ? { ...s, unread: false } : s));
      persist(sessions, state.activeSessionId, state.groups);
      return { sessions, splitSessionId: id };
    });
  },

  closeSplit: () => {
    set({ splitSessionId: null });
  },

  addMessage: (sessionId, msg) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target) return state;

      const now = Date.now();
      const hadUserMessage = target.messages.some((m) => m.role === "user");
      const messages = [...target.messages, msg];

      const derivedText = msg.role === "user" ? textContent(msg.content).trim() : "";
      const title =
        target.title === DEFAULT_TITLE && !hadUserMessage && msg.role === "user"
          ? derivedText.length > 0
            ? deriveTitle(derivedText)
            : "Image attachment"
          : target.title;

      const sessions = state.sessions.map((session) =>
        session.id === sessionId ? { ...session, messages, title, updatedAt: now } : session
      );

      persist(sessions, state.activeSessionId, state.groups);
      return withMirror(state, sessionId, { sessions }, messages);
    });
  },

  updateLastMessage: (sessionId, patch) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target || target.messages.length === 0) {
        return state;
      }
      return applyMessagePatch(state, sessionId, target.messages.length - 1, patch);
    });
  },

  updateMessageAt: (sessionId, index, patch) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target || index < 0 || index >= target.messages.length) {
        return state;
      }
      return applyMessagePatch(state, sessionId, index, patch);
    });
  },

  removeLastMessage: (sessionId) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target || target.messages.length === 0) {
        return state;
      }

      const now = Date.now();
      const messages = target.messages.slice(0, -1);
      const sessions = state.sessions.map((session) =>
        session.id === sessionId ? { ...session, messages, updatedAt: now } : session
      );

      persist(sessions, state.activeSessionId, state.groups);
      return withMirror(state, sessionId, { sessions }, messages);
    });
  },

  truncateFromIndex: (sessionId, index) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target) return state;

      const now = Date.now();
      const messages = target.messages.slice(0, index);
      const sessions = state.sessions.map((session) =>
        session.id === sessionId ? { ...session, messages, updatedAt: now } : session
      );

      persist(sessions, state.activeSessionId, state.groups);
      return withMirror(state, sessionId, { sessions }, messages);
    });
  },

  clear: (sessionId) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target) return state;

      const sessions = state.sessions.map((session) =>
        session.id === sessionId ? { ...session, messages: [] } : session
      );
      persist(sessions, state.activeSessionId, state.groups);
      return withMirror(state, sessionId, { sessions }, []);
    });
  },

  replaceMessages: (sessionId, messages) => {
    set((state) => {
      const target = state.sessions.find((s) => s.id === sessionId);
      if (!target) return state;

      const now = Date.now();
      const sessions = state.sessions.map((session) =>
        session.id === sessionId ? { ...session, messages, updatedAt: now } : session
      );
      persist(sessions, state.activeSessionId, state.groups);
      return withMirror(state, sessionId, { sessions }, messages);
    });
  },
}));

/** Attaches the `messages` mirror to a mutation's partial state when (and
 * only when) the mutated session is the active one — a split-pane session's
 * mutations must never leak into the primary pane's mirror. */
function withMirror(
  state: SessionStore,
  sessionId: string,
  partial: Pick<SessionStore, "sessions">,
  messages: ChatMessage[]
): Partial<SessionStore> {
  return sessionId === state.activeSessionId ? { ...partial, messages } : partial;
}

/** Shared body of `updateLastMessage`/`updateMessageAt`: patch one message
 * of `sessionId`'s transcript immutably and persist. `index` must already be
 * validated by the caller. */
function applyMessagePatch(
  state: SessionStore,
  sessionId: string,
  index: number,
  patch: Partial<ChatMessage>
): Partial<SessionStore> {
  const now = Date.now();
  let updatedMessages: ChatMessage[] = [];
  const sessions = state.sessions.map((session) => {
    if (session.id !== sessionId) return session;

    const messages = session.messages.slice();
    messages[index] = { ...messages[index], ...patch };
    updatedMessages = messages;

    return { ...session, messages, updatedAt: now };
  });

  persist(sessions, state.activeSessionId, state.groups);
  return withMirror(state, sessionId, { sessions }, updatedMessages);
}
