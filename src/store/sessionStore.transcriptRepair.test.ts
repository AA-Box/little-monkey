import { beforeEach, describe, expect, it, vi } from "vitest";

// Same mocking shape as `sessionStore.subagentRuns.test.ts`/`sessionStore.split.test.ts`
// — no real Tauri shell under vitest's node environment.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import { hydrateSessions, useSessionStore } from "./sessionStore";
import type { ChatMessage } from "../lib/llamaClient";

function makeSession(id: string) {
  const now = Date.now();
  return {
    id,
    title: `session ${id}`,
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    workspacePath: null,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  useSessionStore.setState({
    sessions: [],
    groups: [],
    activeSessionId: "",
    splitSessionId: null,
    messages: [],
    runningTurns: {},
    persistError: null,
  });
});

// The transcript-validity repair this feature depends on: every assistant
// `tool_calls` entry must have a matching `tool` result once hydrated, even
// if the app crashed/was force-quit/lost power mid-turn (most plausibly
// while a long-running `task`/subagent round trip was still in flight — see
// `subagent.ts`'s `MAX_SUBAGENT_ITERATIONS`) and the persisted transcript was
// left with a dangling `tool_calls` entry. Without this repair, the next
// turn's `wireHistory` would send a provider that malformed history, which
// several providers reject outright — permanently breaking the session.
describe("normalizeSession / repairDanglingToolCalls", () => {
  it("synthesizes an orphaned-tool-call error result for a tool_calls entry with no matching tool message at all", async () => {
    const messages: unknown[] = [
      { role: "user", content: "delegate this to a subagent" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-1", type: "function", function: { name: "task", arguments: "{}" } }],
      },
      // App crashed here — no matching tool result was ever appended.
    ];

    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [{ ...makeSession("crashed"), messages }],
        activeSessionId: "crashed",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "crashed");
    expect(session?.messages).toHaveLength(3);
    const synthesized = session?.messages[2] as ChatMessage;
    expect(synthesized.role).toBe("tool");
    expect(synthesized.tool_call_id).toBe("call-1");
    expect(JSON.parse(synthesized.content as string)).toHaveProperty("error");
    // Must not be misreported as a user-initiated cancellation — the app
    // crashed, the user never clicked Stop.
    expect(synthesized.content).not.toContain("Cancelled by the user");
  });

  it("fills in only the specific missing tool_call_id when some (but not all) results already exist", async () => {
    const messages: unknown[] = [
      { role: "user", content: "do two things" },
      {
        role: "assistant",
        content: "",
        tool_calls: [
          { id: "call-a", type: "function", function: { name: "read_file", arguments: "{}" } },
          { id: "call-b", type: "function", function: { name: "task", arguments: "{}" } },
        ],
      },
      // Only call-a's result made it to disk before the crash.
      { role: "tool", tool_call_id: "call-a", content: "file contents" },
    ];

    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [{ ...makeSession("partial"), messages }],
        activeSessionId: "partial",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "partial");
    expect(session?.messages).toHaveLength(4);
    expect((session?.messages[2] as ChatMessage).tool_call_id).toBe("call-a");
    const repaired = session?.messages[3] as ChatMessage;
    expect(repaired.role).toBe("tool");
    expect(repaired.tool_call_id).toBe("call-b");
  });

  it("leaves an already-valid transcript completely untouched", async () => {
    const messages: unknown[] = [
      { role: "user", content: "hi" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-1", type: "function", function: { name: "read_file", arguments: "{}" } }],
      },
      { role: "tool", tool_call_id: "call-1", content: "contents" },
      { role: "assistant", content: "done" },
    ];

    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [{ ...makeSession("clean"), messages }],
        activeSessionId: "clean",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "clean");
    expect(session?.messages).toEqual(messages);
  });

  it("repairs dangling tool_calls in an earlier round even when a later round is itself complete", async () => {
    const messages: unknown[] = [
      { role: "user", content: "step 1" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-early", type: "function", function: { name: "task", arguments: "{}" } }],
      },
      // Crash happened mid-round-1, but somehow a later, unrelated round
      // was appended too (e.g. a different code path) — repair must still
      // find and fix the earlier dangling entry, not just the last message.
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-later", type: "function", function: { name: "read_file", arguments: "{}" } }],
      },
      { role: "tool", tool_call_id: "call-later", content: "ok" },
    ];

    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [{ ...makeSession("mixed"), messages }],
        activeSessionId: "mixed",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "mixed");
    // Synthetic result for call-early inserted right after its assistant
    // message, before the later round begins.
    expect(session?.messages.map((m) => m.role)).toEqual(["user", "assistant", "tool", "assistant", "tool"]);
    expect((session?.messages[2] as ChatMessage).tool_call_id).toBe("call-early");
    expect((session?.messages[4] as ChatMessage).tool_call_id).toBe("call-later");
  });
});
