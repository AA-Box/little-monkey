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
  // The REAL implementation (not a spy) — this is a pure function, and the
  // allowlist-enforcement tests below need `runSubagentTask` to apply the
  // exact same logic `agentLoop.ts`'s parent loop does, not a mock that
  // always says "allowed".
  isToolCallAllowed: (toolCall: { function: { name: string } }, toolsForTurn: { function: { name: string } }[]) =>
    toolsForTurn.some((tool) => tool.function.name === toolCall.function.name),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: "Cancelled by the user" }),
  stringifyToolError: (err: unknown) => JSON.stringify({ error: err instanceof Error ? err.message : String(err) }),
}));

import { MAX_SUBAGENT_ITERATIONS, runSubagentTask, type RunSubagentTaskParams } from "./subagent";
import type { ResolvedTarget, RiskAnnotationContext } from "./turnEngine";
import type { ToolCall } from "./llamaClient";
import { selectSubagentRun, useSubagentStore } from "../store/subagentStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useSettingsStore } from "../store/settingsStore";

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

// Slice 3: `code`-profile subagents can call write_file/edit_file/run_shell —
// this is the crux pairing that makes that safe (see the design doc and
// `RunSubagentTaskParams`'s own doc comments): every one of the child's own
// tool calls must carry the PARENT's checkpoint id (so writes land in the
// parent turn's checkpoint) but THIS run's own turn id (so Rust's per-turn
// cancellation/permission maps scope to the subagent, not the parent), and
// its `description` threaded through as `agentLabel` for permission-prompt
// attribution (see `turnEngine.ts`'s `executeToolCall` and `permissions.rs`'s
// `PermissionRequestPayload.agent_label`).
describe("runSubagentTask / code-profile checkpoint_id + turn_id + agent_label pairing", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("passes the PARENT's checkpoint id, this run's OWN turn id, and its description as agentLabel to every child tool call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-w")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Wrote 3 bytes to a.txt");

    await runSubagentTask(
      baseParams({
        profile: "code",
        parentCheckpointId: "parent-checkpoint-1",
        taskId: "child-turn-1",
        description: "refactor auth",
      })
    );

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    const args = executeToolCallMock.mock.calls[0];
    // executeToolCall(toolCall, checkpointId, turnId, mcpRegistry, signal, risk, attachedStackNames, subagent, agentLabel)
    expect(args[1]).toBe("parent-checkpoint-1");
    expect(args[2]).toBe("child-turn-1");
    expect(args[2]).not.toBe(args[1]);
    expect(args[8]).toBe("refactor auth");
  });

  it("still passes the same pairing through for an explore-profile run (harmless — none of its tools are permission-gated mutations)", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("grep", "call-g")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("grep result");

    await runSubagentTask(baseParams({ profile: "explore", parentCheckpointId: "parent-checkpoint-2", taskId: "child-turn-2", description: "find X" }));

    const args = executeToolCallMock.mock.calls[0];
    expect(args[1]).toBe("parent-checkpoint-2");
    expect(args[2]).toBe("child-turn-2");
    expect(args[8]).toBe("find X");
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

// Slice 4: per-subagent token usage — every `attemptStream` call's own
// `usage` (populated regardless of `recordUsage: false`, see that field's
// doc comment on `AttemptResult`) must accumulate into `subagentStore`, not
// just be discarded now that `recordUsage` keeps it out of `useUsageStore`.
describe("runSubagentTask / per-subagent usage accounting (slice 4)", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("accumulates usage from every attemptStream call into subagentStore, keyed the same as the run", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [toolCall("read_file")],
        streamError: null,
        contentStarted: true,
        usage: { promptTokens: 100, completionTokens: 20, totalTokens: 120 },
      })
      .mockResolvedValueOnce({
        content: "done",
        toolCalls: [],
        streamError: null,
        contentStarted: true,
        usage: { promptTokens: 50, completionTokens: 10, totalTokens: 60 },
      });
    executeToolCallMock.mockResolvedValue("file contents");

    await runSubagentTask(baseParams({ toolCallId: "call-usage-1" }));

    const run = selectSubagentRun("call-usage-1")(useSubagentStore.getState());
    expect(run?.usage).toEqual({ promptTokens: 150, completionTokens: 30, totalTokens: 180 });
  });

  it("leaves usage undefined when no attemptStream call ever reports one", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ toolCallId: "call-usage-2" }));

    expect(selectSubagentRun("call-usage-2")(useSubagentStore.getState())?.usage).toBeUndefined();
  });
});

// Slice 4: optional per-profile model override — `resolveSubagentTarget`
// (private to subagent.ts) is exercised indirectly here through what target
// actually reaches `attemptStream`.
describe("runSubagentTask / per-profile model override (slice 4)", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
    useSettingsStore.setState({ subagentProfileModels: {} });
  });

  it("uses the parent's own target unchanged when no override is configured for the profile", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ profile: "explore" }));

    expect(attemptStreamMock.mock.calls[0][0]).toBe(fakeTarget);
  });

  it("swaps in the configured override target for a matching profile", async () => {
    useSettingsStore.getState().setSubagentProfileModel("explore", { providerId: "openrouter", model: "cheap-model" });
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ profile: "explore" }));

    expect(attemptStreamMock.mock.calls[0][0]).toEqual({ kind: "provider", providerId: "openrouter", model: "cheap-model" });
  });

  it("does not apply an override configured for a different profile", async () => {
    useSettingsStore.getState().setSubagentProfileModel("code", { providerId: "openrouter", model: "cheap-model" });
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ profile: "explore" }));

    expect(attemptStreamMock.mock.calls[0][0]).toBe(fakeTarget);
  });
});

// Review finding: a child model that emits a tool_call name outside its
// profile's own offered tool list (`toolsForProfile`) must never have it
// actually dispatched — the exact same `isToolCallAllowed` gate
// `agentLoop.ts`'s parent loop applies, reused here. Without this, a real
// risk with local/quantized models that don't strictly respect the offered
// tool schema (the same risk `agentLoop.ts`'s own `isToolCallAllowed` doc
// comment calls out) would let an 'explore'-profile subagent's model
// hallucinate a `write_file`/`edit_file`/`run_shell` call and have it
// actually executed.
describe("runSubagentTask / tool allowlist enforcement", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("rejects (without executing) a write_file call from an explore-profile subagent", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-rogue")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    const result = await runSubagentTask(baseParams({ profile: "explore" }));

    expect(result).toBe("done");
    expect(executeToolCallMock).not.toHaveBeenCalled();
  });

  it("still feeds the rejected call a tool-error result — the transcript-validity invariant holds even for a disallowed call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("run_shell", "call-rogue")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    await runSubagentTask(baseParams({ profile: "explore", toolCallId: "call-allowlist-1" }));

    const run = selectSubagentRun("call-allowlist-1")(useSubagentStore.getState());
    const toolMessage = run?.liveMessages.find((m) => m.role === "tool" && m.tool_call_id === "call-rogue");
    expect(toolMessage).toBeDefined();
    const parsed = JSON.parse(toolMessage!.content as string) as { error: string };
    expect(parsed.error).toContain("run_shell");
    expect(parsed.error).toContain("explore");
  });

  it("still allows an in-profile call through for the same run that also rejects an out-of-profile one", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [toolCall("grep", "call-ok"), toolCall("write_file", "call-rogue")],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("grep result");

    await runSubagentTask(baseParams({ profile: "explore" }));

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    expect(executeToolCallMock.mock.calls[0][0]).toEqual(toolCall("grep", "call-ok"));
  });

  it("allows write_file/edit_file/run_shell for a code-profile subagent — the allowlist gate is profile-aware, not a blanket rejection", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-w")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Wrote 3 bytes to a.txt");

    await runSubagentTask(baseParams({ profile: "code" }));

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
  });
});

// Review finding: a code-profile subagent's mutating tool calls must get the
// same advisory risk classification the parent turn's own equivalent calls
// would — threaded through via `RunSubagentTaskParams.risk`.
describe("runSubagentTask / risk-annotation threading", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("passes the given risk context through to every child executeToolCall call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-w")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Wrote 3 bytes to a.txt");
    const risk: RiskAnnotationContext = { enabled: true, cache: new Map(), classify: vi.fn() };

    await runSubagentTask(baseParams({ profile: "code", risk }));

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    // executeToolCall(toolCall, checkpointId, turnId, mcpRegistry, signal, risk, attachedStackNames, subagent, agentLabel)
    expect(executeToolCallMock.mock.calls[0][5]).toBe(risk);
  });

  it("passes undefined risk through unchanged when the caller never built one (risk annotations off)", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-w")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Wrote 3 bytes to a.txt");

    await runSubagentTask(baseParams({ profile: "code" }));

    expect(executeToolCallMock.mock.calls[0][5]).toBeUndefined();
  });
});

// Review finding: without this, `runVerificationPhase` in `agentLoop.ts`
// never fires for a turn where every mutation happened inside a delegated
// `task` call, since that round's own top-level `toolCalls` only ever
// contains the single `task` entry.
describe("runSubagentTask / onMutatedPath reporting", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("reports the path of a successful write_file call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [{ id: "call-w", type: "function", function: { name: "write_file", arguments: JSON.stringify({ path: "a.txt", content: "x" }) } }],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Wrote 1 bytes to a.txt");
    const onMutatedPath = vi.fn();

    await runSubagentTask(baseParams({ profile: "code", onMutatedPath }));

    expect(onMutatedPath).toHaveBeenCalledTimes(1);
    expect(onMutatedPath).toHaveBeenCalledWith("a.txt");
  });

  it("reports the path of a successful edit_file call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [
          {
            id: "call-e",
            type: "function",
            function: { name: "edit_file", arguments: JSON.stringify({ path: "b.txt", old_string: "x", new_string: "y" }) },
          },
        ],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("Edited b.txt");
    const onMutatedPath = vi.fn();

    await runSubagentTask(baseParams({ profile: "code", onMutatedPath }));

    expect(onMutatedPath).toHaveBeenCalledTimes(1);
    expect(onMutatedPath).toHaveBeenCalledWith("b.txt");
  });

  it("does not report a failed write_file call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [{ id: "call-w", type: "function", function: { name: "write_file", arguments: JSON.stringify({ path: "a.txt", content: "x" }) } }],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue(JSON.stringify({ error: "Permission denied" }));
    const onMutatedPath = vi.fn();

    await runSubagentTask(baseParams({ profile: "code", onMutatedPath }));

    expect(onMutatedPath).not.toHaveBeenCalled();
  });

  it("does not report a read-only tool call", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("grep", "call-g")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("grep result");
    const onMutatedPath = vi.fn();

    await runSubagentTask(baseParams({ profile: "code", onMutatedPath }));

    expect(onMutatedPath).not.toHaveBeenCalled();
  });

  it("does not report a write_file call rejected by the allowlist gate", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [{ id: "call-rogue", type: "function", function: { name: "write_file", arguments: JSON.stringify({ path: "a.txt", content: "x" }) } }],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    const onMutatedPath = vi.fn();

    await runSubagentTask(baseParams({ profile: "explore", onMutatedPath }));

    expect(executeToolCallMock).not.toHaveBeenCalled();
    expect(onMutatedPath).not.toHaveBeenCalled();
  });
});
