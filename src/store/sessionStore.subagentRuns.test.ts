import { beforeEach, describe, expect, it, vi } from "vitest";

// Same mocking shape as `sessionStore.split.test.ts` — no real Tauri shell
// under vitest's node environment.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import { hydrateSessions, sessionMessages, useSessionStore, type ChatSession, type SubagentRunMeta } from "./sessionStore";
import type { ChatMessage } from "../lib/llamaClient";

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
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  const a = makeSession("a");
  useSessionStore.setState({
    sessions: [a],
    groups: [],
    activeSessionId: "a",
    splitSessionId: null,
    messages: a.messages,
    runningTurns: {},
    persistError: null,
  });
});

describe("normalizeSession subagentRuns default", () => {
  it("defaults subagentRuns to {} for a persisted session predating the field", async () => {
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
            // No `subagentRuns` field at all — simulates a blob saved
            // before this feature existed.
          },
        ],
        activeSessionId: "old",
        groups: [],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().sessions.find((s) => s.id === "old")?.subagentRuns).toEqual({});
  });

  it("round-trips a valid persisted subagentRuns blob, dropping malformed entries", async () => {
    const goodChild: ChatMessage = { role: "assistant", content: "Found 3 callers of X." };
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old2",
            title: "Old session 2",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            subagentRuns: {
              "call-good": [goodChild],
              "call-bad": "not an array",
              "call-mixed": [goodChild, { role: "bogus", content: 42 }],
            },
          },
        ],
        activeSessionId: "old2",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "old2");
    expect(session?.subagentRuns["call-good"]).toEqual([goodChild]);
    expect(session?.subagentRuns["call-bad"]).toBeUndefined();
    // The malformed second entry in "call-mixed" is dropped, the valid one kept.
    expect(session?.subagentRuns["call-mixed"]).toEqual([goodChild]);
  });
});

describe("setSubagentRun", () => {
  it("persists a finished child transcript under the given taskId", () => {
    const transcript: ChatMessage[] = [
      { role: "user", content: "find every caller of X" },
      { role: "assistant", content: "Found 3 callers of X." },
    ];

    useSessionStore.getState().setSubagentRun("a", "call-1", transcript);

    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.subagentRuns["call-1"]).toEqual(transcript);
  });

  it("is a no-op for an unknown session id", () => {
    const before = useSessionStore.getState().sessions;
    useSessionStore.getState().setSubagentRun("does-not-exist", "call-1", [{ role: "user", content: "x" }]);
    expect(useSessionStore.getState().sessions).toBe(before);
  });

  it("persists final run stats under subagentRunMeta when provided, leaving them untouched when omitted", () => {
    const meta: SubagentRunMeta = {
      status: "done",
      description: "find every caller of X",
      profile: "explore",
      startedAt: 1_000,
      finishedAt: 5_000,
      toolCallCount: 3,
      usage: { promptTokens: 100, completionTokens: 20, totalTokens: 120 },
    };

    useSessionStore.getState().setSubagentRun("a", "call-1", [], meta);
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.subagentRunMeta?.["call-1"]).toEqual(meta);

    // A meta-less write (defensive path: live store entry already gone)
    // updates the transcript without clobbering existing stats.
    useSessionStore.getState().setSubagentRun("a", "call-1", [{ role: "user", content: "x" }]);
    expect(useSessionStore.getState().sessions.find((s) => s.id === "a")?.subagentRunMeta?.["call-1"]).toEqual(meta);
  });
});

describe("normalizeSession subagentRunMeta default", () => {
  it("defaults to {} for pre-field sessions and drops malformed entries", async () => {
    const goodMeta: SubagentRunMeta = {
      status: "done",
      description: "audit i18n",
      profile: "code",
      startedAt: 1,
      finishedAt: 2,
      toolCallCount: 1,
      usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
    };
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old3",
            title: "Old session 3",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
            subagentRunMeta: {
              "call-good": goodMeta,
              "call-bad-status": { ...goodMeta, status: "running" },
              "call-bad-times": { ...goodMeta, startedAt: "yesterday" },
              "call-bad-usage": { ...goodMeta, usage: { totalTokens: "many" } },
            },
          },
          {
            id: "pre-field",
            title: "Pre-field session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: null,
            workspacePath: null,
          },
        ],
        activeSessionId: "old3",
        groups: [],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions.find((s) => s.id === "old3");
    expect(session?.subagentRunMeta?.["call-good"]).toEqual(goodMeta);
    expect(session?.subagentRunMeta?.["call-bad-status"]).toBeUndefined();
    expect(session?.subagentRunMeta?.["call-bad-times"]).toBeUndefined();
    expect(session?.subagentRunMeta?.["call-bad-usage"]).toBeUndefined();
    expect(useSessionStore.getState().sessions.find((s) => s.id === "pre-field")?.subagentRunMeta).toEqual({});
  });
});

describe("wire-payload isolation (subagent transcripts never leak into the next turn's request)", () => {
  it("sessionMessages — what agentLoop.ts feeds into wireHistory — contains only the task tool's report string, never the child's liveMessages", () => {
    const secretChildContent = "SECRET_CHILD_EXPLORATION_DETAIL_THAT_MUST_NEVER_REACH_THE_PARENT_WIRE_PAYLOAD";
    const bigChildTranscript: ChatMessage[] = [
      { role: "user", content: "find every caller of X" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "child-call-1", type: "function", function: { name: "grep", arguments: "{}" } }],
      },
      { role: "tool", tool_call_id: "child-call-1", content: secretChildContent },
      { role: "assistant", content: "Found 3 callers of X." },
    ];

    const parentMessages: ChatMessage[] = [
      { role: "user", content: "delegate this to a subagent" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-1", type: "function", function: { name: "task", arguments: "{}" } }],
      },
      { role: "tool", tool_call_id: "call-1", content: "Found 3 callers of X." },
    ];

    const session = makeSession("wire-test", { messages: parentMessages, subagentRuns: { "call-1": bigChildTranscript } });
    useSessionStore.setState((state) => ({ sessions: [...state.sessions, session] }));

    const wireRelevantMessages = sessionMessages("wire-test");

    expect(wireRelevantMessages).toEqual(parentMessages);
    expect(JSON.stringify(wireRelevantMessages)).not.toContain(secretChildContent);
    expect(JSON.stringify(wireRelevantMessages)).not.toContain("child-call-1");
  });
});
