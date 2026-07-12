import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// The store persists through Tauri IPC and subscribes to window events on
// hydrate — none of which exists under vitest's node environment. `invoke`
// forwards to a reassignable mock (rather than a fixed inline `vi.fn`) so
// individual tests below can make `sessions_load` return a specific blob,
// same pattern as `promptStore.test.ts`.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import { hydrateSessions, useSessionStore, type ChatSession } from "./sessionStore";
import { usePromptStore } from "./promptStore";

function makeSession(id: string, overrides: Partial<ChatSession> = {}): ChatSession {
  const now = Date.now();
  return {
    id,
    title: `session ${id}`,
    messages: [],
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    ...overrides,
  };
}

/** Resets the singleton store to two known sessions, `a` active, no split. */
function seed(...extra: ChatSession[]): void {
  const a = makeSession("a");
  const b = makeSession("b");
  useSessionStore.setState({
    sessions: [a, b, ...extra],
    groups: [],
    activeSessionId: "a",
    splitSessionId: null,
    messages: a.messages,
    runningTurns: {},
    persistError: null,
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  seed();
});

describe("openSplit", () => {
  it("opens another session in the split pane and clears its unread flag", () => {
    seed(makeSession("c", { unread: true }));
    useSessionStore.getState().openSplit("c");
    const state = useSessionStore.getState();
    expect(state.splitSessionId).toBe("c");
    expect(state.sessions.find((s) => s.id === "c")?.unread).toBe(false);
  });

  it("refuses to open the active session in the split pane", () => {
    useSessionStore.getState().openSplit("a");
    expect(useSessionStore.getState().splitSessionId).toBeNull();
  });

  it("no-ops for an unknown session id", () => {
    useSessionStore.getState().openSplit("nope");
    expect(useSessionStore.getState().splitSessionId).toBeNull();
  });
});

describe("one-transcript-per-pane invariant", () => {
  it("switchSession onto the split session closes the split pane", () => {
    useSessionStore.getState().openSplit("b");
    useSessionStore.getState().switchSession("b");
    const state = useSessionStore.getState();
    expect(state.activeSessionId).toBe("b");
    expect(state.splitSessionId).toBeNull();
  });

  it("deleteSession promoting the split session to primary closes the split pane", () => {
    // Make "b" the most recently updated so it gets promoted when "a" dies.
    useSessionStore.setState((state) => ({
      sessions: state.sessions.map((s) => (s.id === "b" ? { ...s, updatedAt: Date.now() + 1000 } : s)),
    }));
    useSessionStore.getState().openSplit("b");
    useSessionStore.getState().deleteSession("a");
    const state = useSessionStore.getState();
    expect(state.activeSessionId).toBe("b");
    expect(state.splitSessionId).toBeNull();
  });

  it("deleting the split session itself closes the split pane", () => {
    useSessionStore.getState().openSplit("b");
    useSessionStore.getState().deleteSession("b");
    expect(useSessionStore.getState().splitSessionId).toBeNull();
  });
});

describe("transcript mutations with a split pane open", () => {
  it("split-session messages never leak into the active pane's mirror", () => {
    useSessionStore.getState().openSplit("b");
    useSessionStore.getState().addMessage("b", { role: "user", content: "to split" });
    const state = useSessionStore.getState();
    expect(state.messages).toHaveLength(0);
    expect(state.sessions.find((s) => s.id === "b")?.messages).toHaveLength(1);
  });

  it("active-session messages update the mirror", () => {
    useSessionStore.getState().addMessage("a", { role: "user", content: "to active" });
    expect(useSessionStore.getState().messages).toHaveLength(1);
  });

  it("concurrent streaming into both panes patches the right transcripts", () => {
    useSessionStore.getState().openSplit("b");
    const store = useSessionStore.getState();
    store.addMessage("a", { role: "assistant", content: "" });
    store.addMessage("b", { role: "assistant", content: "" });
    // Interleaved deltas, as two in-flight turns produce.
    store.updateLastMessage("a", { content: "alpha" });
    store.updateLastMessage("b", { content: "beta" });
    store.updateLastMessage("a", { content: "alpha alpha" });
    const state = useSessionStore.getState();
    const last = (id: string) => {
      const messages = state.sessions.find((s) => s.id === id)?.messages ?? [];
      return messages[messages.length - 1]?.content;
    };
    expect(last("a")).toBe("alpha alpha");
    expect(last("b")).toBe("beta");
  });

  it("mutations for a deleted session are dropped", () => {
    useSessionStore.getState().deleteSession("b");
    useSessionStore.getState().addMessage("b", { role: "user", content: "ghost" });
    expect(useSessionStore.getState().sessions.some((s) => s.id === "b")).toBe(false);
  });
});

describe("setSessionPersona", () => {
  it("sets the persona id for the given session", () => {
    useSessionStore.getState().setSessionPersona("a", "persona-1");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.personaId).toBe("persona-1");
  });

  it("clears the persona with null", () => {
    useSessionStore.getState().setSessionPersona("a", "persona-1");
    useSessionStore.getState().setSessionPersona("a", null);
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.personaId).toBeNull();
  });

  it("only affects the targeted session", () => {
    useSessionStore.getState().setSessionPersona("a", "persona-1");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "b")?.personaId).toBeNull();
  });
});

describe("newSession applies the library's default persona", () => {
  afterEach(() => {
    usePromptStore.setState({ defaultPersonaId: null });
  });

  it("sets personaId to promptStore.defaultPersonaId on a freshly created session", () => {
    usePromptStore.setState({ defaultPersonaId: "persona-default" });
    useSessionStore.getState().newSession();
    const active = useSessionStore.getState().sessions.find((s) => s.id === useSessionStore.getState().activeSessionId);
    expect(active?.personaId).toBe("persona-default");
  });

  it("leaves personaId null when there is no default persona", () => {
    usePromptStore.setState({ defaultPersonaId: null });
    useSessionStore.getState().newSession();
    const active = useSessionStore.getState().sessions.find((s) => s.id === useSessionStore.getState().activeSessionId);
    expect(active?.personaId).toBeNull();
  });
});

describe("hydrateSessions persona default", () => {
  it("defaults personaId to null for a persisted session predating the field", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old",
            title: "Old session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            // No `personaId` field at all — simulates a blob saved before
            // this feature existed.
          },
        ],
        activeSessionId: "old",
        groups: [],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().sessions.find((s) => s.id === "old")?.personaId).toBeNull();
  });
});

describe("toggleAttachedStack", () => {
  it("attaches a stack id not yet attached", () => {
    useSessionStore.getState().toggleAttachedStack("a", "stack-1");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.attachedStackIds).toEqual(["stack-1"]);
  });

  it("detaches an already-attached stack id", () => {
    useSessionStore.getState().toggleAttachedStack("a", "stack-1");
    useSessionStore.getState().toggleAttachedStack("a", "stack-1");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.attachedStackIds).toEqual([]);
  });

  it("supports multiple attached stacks at once", () => {
    useSessionStore.getState().toggleAttachedStack("a", "stack-1");
    useSessionStore.getState().toggleAttachedStack("a", "stack-2");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.attachedStackIds).toEqual(["stack-1", "stack-2"]);
  });

  it("only affects the targeted session", () => {
    useSessionStore.getState().toggleAttachedStack("a", "stack-1");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "b")?.attachedStackIds).toEqual([]);
  });
});

describe("toggleDocChatMode", () => {
  it("turns doc-chat mode on for a session that starts with it off", () => {
    useSessionStore.getState().toggleDocChatMode("a");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.docChatMode).toBe(true);
  });

  it("turns it back off on a second toggle", () => {
    useSessionStore.getState().toggleDocChatMode("a");
    useSessionStore.getState().toggleDocChatMode("a");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.docChatMode).toBe(false);
  });

  it("only affects the targeted session", () => {
    useSessionStore.getState().toggleDocChatMode("a");
    expect(useSessionStore.getState().sessions.find((s) => s.id === "b")?.docChatMode).toBe(false);
  });
});

describe("hydrateSessions docChatMode default", () => {
  it("defaults docChatMode to false for a persisted session predating the field", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old",
            title: "Old session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            personaId: null,
            attachedStackIds: [],
            // No `docChatMode` field at all — simulates a blob saved before
            // this feature existed.
          },
        ],
        activeSessionId: "old",
        groups: [],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().sessions.find((s) => s.id === "old")?.docChatMode).toBe(false);
  });
});

describe("hydrateSessions attachedStackIds default", () => {
  it("defaults attachedStackIds to [] for a persisted session predating the field", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old",
            title: "Old session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            personaId: null,
            // No `attachedStackIds` field at all — simulates a blob saved
            // before this feature existed.
          },
        ],
        activeSessionId: "old",
        groups: [],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().sessions.find((s) => s.id === "old")?.attachedStackIds).toEqual([]);
  });

  it("drops non-string entries from a corrupt persisted attachedStackIds array", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old",
            title: "Old session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            personaId: null,
            attachedStackIds: ["stack-1", 42, null, "stack-2"],
          },
        ],
        activeSessionId: "old",
        groups: [],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().sessions.find((s) => s.id === "old")?.attachedStackIds).toEqual([
      "stack-1",
      "stack-2",
    ]);
  });
});
