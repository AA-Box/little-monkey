import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

// `runSubagentTask` drives its own model->tools->model loop via
// `turnEngine.ts`'s `attemptStream`/`executeToolCall` — mocked here so these
// tests can pin the LOOP's own behavior (termination, iteration cap,
// cancellation, transcript-invariant-on-crash, recordUsage threading)
// without needing a real streaming provider. `turnEngine.test.ts` separately
// covers the `executeToolCall` `task`-branch delegation contract itself.
const attemptStreamMock = vi.fn();
const executeToolCallMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
  executeToolCall: (...args: unknown[]) => executeToolCallMock(...args),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: "Cancelled by the user" }),
  stringifyToolError: (err: unknown) => JSON.stringify({ error: err instanceof Error ? err.message : String(err) }),
}));

import { MAX_SUBAGENT_ITERATIONS, runSubagentTask, type RunSubagentTaskParams } from "./subagent";
import type { ResolvedTarget } from "./turnEngine";
import type { ToolCall } from "./llamaClient";
import { selectSubagentRun, useSubagentStore } from "../store/subagentStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";

const fakeTarget: ResolvedTarget = { kind: "local", baseUrl: "http://localhost:8090" };

function baseParams(overrides: Partial<RunSubagentTaskParams> = {}): RunSubagentTaskParams {
  return {
    sessionId: "session-1",
    parentCheckpointId: null,
    taskId: "child-turn-1",
    description: "find X",
    prompt: "find every caller of X",
    profile: "explore",
    target: fakeTarget,
    ...overrides,
  };
}

function toolCall(name: string, id = "call-1"): ToolCall {
  return { id, type: "function", function: { name, arguments: "{}" } };
}

describe("runSubagentTask / termination", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("returns the child's final assistant reply once it stops requesting tool calls", async () => {
    attemptStreamMock.mockResolvedValue({ content: "Found 3 callers of X.", toolCalls: [], streamError: null, contentStarted: true });

    const result = await runSubagentTask(baseParams());

    expect(result).toBe("Found 3 callers of X.");
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    expect(executeToolCallMock).not.toHaveBeenCalled();
  });

  // Depth-cap-of-1, exercised at this module's own boundary: the tool list
  // it offers the child model (built via `toolsForProfile`, the REAL
  // function — not mocked here) must never include `task`, no matter which
  // profile is requested. See `tools.test.ts` for the exhaustive coverage of
  // `toolsForProfile` itself; this pins that `runSubagentTask` actually uses
  // it rather than some other, possibly-unrestricted tool list.
  it("never offers the task tool to the child model, for either profile", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ profile: "explore" }));
    const exploreTools = attemptStreamMock.mock.calls[0][2] as { function: { name: string } }[];
    expect(exploreTools.some((t) => t.function.name === "task")).toBe(false);

    attemptStreamMock.mockClear();
    await runSubagentTask(baseParams({ profile: "code" }));
    const codeTools = attemptStreamMock.mock.calls[0][2] as { function: { name: string } }[];
    expect(codeTools.some((t) => t.function.name === "task")).toBe(false);
  });

  it("passes recordUsage: false to every attemptStream call — a child attempt must never clobber the parent session's usage ring", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams());

    const args = attemptStreamMock.mock.calls[0];
    // attemptStream(target, wireHistory, tools, signal, effort, sessionId, onDelta, recordUsage)
    expect(args[7]).toBe(false);
  });

  it("passes the same target and effort through unchanged, and the sessionId given", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ effort: "high", sessionId: "session-42" }));

    const args = attemptStreamMock.mock.calls[0];
    expect(args[0]).toBe(fakeTarget);
    expect(args[4]).toBe("high");
    expect(args[5]).toBe("session-42");
  });

  it("returns a stringifyToolError-shaped result when the stream itself errors", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "network broke", contentStarted: false });

    const result = await runSubagentTask(baseParams());

    expect(JSON.parse(result)).toEqual({ error: "network broke" });
  });

  it("runs a tool round trip: executes each requested tool call, feeds results back, then returns the eventual final answer", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "All done.", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("file contents here");

    const result = await runSubagentTask(baseParams());

    expect(result).toBe("All done.");
    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock).toHaveBeenCalledTimes(2);
  });

  it("caps an extremely long final report", async () => {
    const huge = "x".repeat(20_000);
    attemptStreamMock.mockResolvedValue({ content: huge, toolCalls: [], streamError: null, contentStarted: true });

    const result = await runSubagentTask(baseParams());

    expect(result.length).toBeLessThan(huge.length);
    expect(result).toContain("truncated");
  });
});

// The safety cap this slice's design doc calls out explicitly — a child that
// never settles on a final answer must not loop forever.
describe("runSubagentTask / MAX_SUBAGENT_ITERATIONS cap", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
    executeToolCallMock.mockResolvedValue("tool result");
  });

  it("stops after MAX_SUBAGENT_ITERATIONS round trips and returns a tool-error result instead of looping forever", async () => {
    // Every attempt requests another tool call — a runaway/looping model.
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true });

    const result = await runSubagentTask(baseParams());

    expect(attemptStreamMock).toHaveBeenCalledTimes(MAX_SUBAGENT_ITERATIONS);
    const parsed = JSON.parse(result) as { error: string };
    expect(parsed.error).toContain(String(MAX_SUBAGENT_ITERATIONS));
  });
});

describe("runSubagentTask / cancellation", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("returns CANCELLED_TOOL_RESULT immediately when the parent signal is already aborted, without calling attemptStream", async () => {
    const controller = new AbortController();
    controller.abort();

    const result = await runSubagentTask(baseParams({ parentSignal: controller.signal }));

    expect(result).toBe(JSON.stringify({ error: "Cancelled by the user" }));
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });
});

// The transcript-validity invariant this whole feature depends on:
// `runSubagentTask`'s own try/catch must swallow ANY exception (not just a
// stream error reported through `AttemptResult.streamError`) and always
// return a string, never let it propagate — `turnEngine.ts`'s `executeToolCall`
// depends on this to keep every `task` tool_call paired with a tool result.
describe("runSubagentTask / never throws", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("catches an exception thrown by attemptStream and returns a stringifyToolError-shaped result", async () => {
    attemptStreamMock.mockRejectedValue(new Error("boom"));

    const result = await runSubagentTask(baseParams());

    expect(JSON.parse(result)).toEqual({ error: "boom" });
  });

  it("catches an exception thrown by executeToolCall mid-round-trip and returns a stringifyToolError-shaped result", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true });
    executeToolCallMock.mockRejectedValue(new Error("child tool crashed"));

    const result = await runSubagentTask(baseParams());

    expect(JSON.parse(result)).toEqual({ error: "child tool crashed" });
  });
});

// Design doc slice 1: subagent execution is sequential, not parallel — the
// parent's own tool-calling loop (`agentLoop.ts`) awaits each `task` call
// one at a time (parallelism across multiple `task` calls in one round trip
// is slice 3). This is exercised here at the level this module controls:
// multiple sequential `runSubagentTask` invocations never overlap in their
// own internal tool-call round trips.
describe("runSubagentTask / sequential execution", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("two sequential runs never interleave — the second only starts after the first fully resolves", async () => {
    const order: string[] = [];
    attemptStreamMock.mockImplementation(async () => {
      order.push("attempt-start");
      await Promise.resolve();
      order.push("attempt-end");
      return { content: "done", toolCalls: [], streamError: null, contentStarted: true };
    });

    await runSubagentTask(baseParams({ taskId: "run-1" }));
    await runSubagentTask(baseParams({ taskId: "run-2" }));

    expect(order).toEqual(["attempt-start", "attempt-end", "attempt-start", "attempt-end"]);
  });
});

// Slice 2: `runSubagentTask` drives `subagentStore` (live status for
// `SubagentRow`) and `sessionStore.setSubagentRun` (persistence across a
// restart) as it streams — see `subagentStore.test.ts` for the store's own
// reducer-level tests; these pin that `runSubagentTask` actually calls
// through to it at the right moments, keyed by `toolCallId` (NOT `taskId` —
// see `RunSubagentTaskParams.toolCallId`'s doc comment).
function makeStoreTestSession(id: string): ChatSession {
  const now = Date.now();
  return {
    id,
    title: "test",
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
    subagentRuns: {},
  };
}

describe("runSubagentTask / subagentStore + sessionStore integration", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("registers the run as 'running' (keyed by toolCallId) before the first attemptStream call resolves", async () => {
    let resolveAttempt!: (value: { content: string; toolCalls: ToolCall[]; streamError: null; contentStarted: boolean }) => void;
    attemptStreamMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveAttempt = resolve;
        })
    );

    const promise = runSubagentTask(baseParams({ taskId: "rust-turn-1", toolCallId: "call-store-1", description: "find X" }));
    await Promise.resolve();
    await Promise.resolve();

    const run = selectSubagentRun("call-store-1")(useSubagentStore.getState());
    expect(run?.status).toBe("running");
    expect(run?.description).toBe("find X");
    expect(run?.profile).toBe("explore");
    expect(run?.toolCallCount).toBe(0);
    // The Rust-facing turn id must NEVER be the store key when a distinct
    // toolCallId was supplied.
    expect(selectSubagentRun("rust-turn-1")(useSubagentStore.getState())).toBeUndefined();

    resolveAttempt({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    await promise;
  });

  it("falls back to taskId as the store key when no toolCallId is supplied", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ taskId: "fallback-key-1" }));

    expect(selectSubagentRun("fallback-key-1")(useSubagentStore.getState())?.status).toBe("done");
  });

  it("bumps toolCallCount and lastActivity per child tool call, then finishes 'done' with a non-empty liveMessages log", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("grep", "call-a")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "All done.", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("grep result");

    const result = await runSubagentTask(baseParams({ toolCallId: "call-store-2" }));

    expect(result).toBe("All done.");
    const run = selectSubagentRun("call-store-2")(useSubagentStore.getState());
    expect(run?.status).toBe("done");
    expect(run?.toolCallCount).toBe(1);
    expect(run?.lastActivity).toContain("grep(");
    expect(run?.liveMessages.length).toBeGreaterThan(0);
  });

  it("finishes 'error' on a stream failure, and 'cancelled' when the parent signal aborts mid-run", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "network broke", contentStarted: false });
    await runSubagentTask(baseParams({ toolCallId: "call-store-error" }));
    expect(selectSubagentRun("call-store-error")(useSubagentStore.getState())?.status).toBe("error");

    attemptStreamMock.mockReset();
    const controller = new AbortController();
    controller.abort();
    await runSubagentTask(baseParams({ toolCallId: "call-store-cancelled", parentSignal: controller.signal }));
    expect(selectSubagentRun("call-store-cancelled")(useSubagentStore.getState())?.status).toBe("cancelled");
  });

  it("persists the finished child transcript into ChatSession.subagentRuns for an existing session", async () => {
    useSessionStore.setState((state) => ({ sessions: [...state.sessions, makeStoreTestSession("sess-store-test")] }));
    attemptStreamMock.mockResolvedValue({ content: "All done.", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ sessionId: "sess-store-test", toolCallId: "call-store-3" }));

    const session = useSessionStore.getState().sessions.find((s) => s.id === "sess-store-test");
    const persisted = session?.subagentRuns["call-store-3"];
    expect(persisted).toBeDefined();
    expect(persisted?.some((m) => m.role === "assistant" && m.content === "All done.")).toBe(true);
  });
});
