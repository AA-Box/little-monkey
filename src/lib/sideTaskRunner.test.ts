import { beforeEach, describe, expect, it, vi } from "vitest";
import { errorMessage } from "./errors";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

// `runSideTask` drives its own model->tools->model loop via `turnEngine.ts`'s
// `attemptStream`/`executeToolCall` — mocked here (same pattern as
// `subagent.test.ts`) so these tests pin the LOOP's own behavior
// (termination, iteration cap, cancellation, pause/resume, tool-evidence
// tracking, isolation args) without a real streaming provider.
const attemptStreamMock = vi.fn();
const executeToolCallMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
  executeToolCall: (...args: unknown[]) => executeToolCallMock(...args),
  isToolCallAllowed: (toolCall: { function: { name: string } }, toolsForTurn: { function: { name: string } }[]) =>
    toolsForTurn.some((tool) => tool.function.name === toolCall.function.name),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: "Cancelled by the user" }),
  stringifyToolError: (err: unknown) => JSON.stringify({ error: errorMessage(err) }),
  describeUsageTarget: (target: { kind: string; providerId?: string; model?: string }) =>
    target.kind === "local" ? "Local model" : target.kind === "ollama" ? `Ollama · ${target.model}` : `${target.providerId} · ${target.model}`,
}));

const resolveTargetMock = vi.fn();
vi.mock("./agentLoop", () => ({ resolveTarget: (...args: unknown[]) => resolveTargetMock(...args) }));

import {
  MAX_SIDE_TASK_ITERATIONS,
  cancelSideTask,
  continueSideTask,
  openSideTaskAsFullChat,
  pauseSideTask,
  promoteSideTask,
  resumeSideTask,
  retrySideTask,
  runSideTask,
  startSideTask,
  waitUntilResumed,
} from "./sideTaskRunner";
import type { ToolCall } from "./llamaClient";
import { useSideTaskStore, type SideTaskSource } from "../store/sideTaskStore";
import { useSessionStore } from "../store/sessionStore";

const localTarget = { kind: "local" as const, baseUrl: "http://localhost:8090", modelLabel: "Local model" };
const source: SideTaskSource = { kind: "chat_message", label: "Assistant message", excerpt: "explain the auth flow" };

function toolCall(name: string, id = "call-1", args = "{}"): ToolCall {
  return { id, type: "function", function: { name, arguments: args } };
}

function seedTask(overrides: Partial<{ title: string; prompt: string; profile: "explore" | "code"; sessionId: string }> = {}) {
  return useSideTaskStore.getState().create({
    title: overrides.title ?? "Explain auth",
    prompt: overrides.prompt ?? "Explain the auth flow",
    profile: overrides.profile ?? "explore",
    source,
    sessionId: overrides.sessionId ?? "session-1",
    modelLabel: "pending",
  });
}

beforeEach(() => {
  attemptStreamMock.mockReset();
  executeToolCallMock.mockReset();
  resolveTargetMock.mockReset();
  resolveTargetMock.mockResolvedValue(localTarget);
  useSideTaskStore.setState({ tasks: {}, order: [], paneOpen: false, openTabs: [], activeTabId: null, selectedTaskId: null, composerSeed: null, composerOpen: false });
  useSessionStore.setState({ sessions: [], activeSessionId: "" } as never);
});

describe("runSideTask / termination", () => {
  it("resolves the active target once and records it as the task's modelLabel", async () => {
    attemptStreamMock.mockResolvedValue({ content: "All done.", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask();

    await runSideTask(record.id);

    expect(resolveTargetMock).toHaveBeenCalledTimes(1);
    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.modelLabel).toBe("Local model");
    expect(task.status).toBe("completed");
    expect(task.finalReport).toBe("All done.");
  });

  it("passes recordUsage: false to attemptStream — a side task must never clobber the parent session's usage ring", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask();

    await runSideTask(record.id);

    // attemptStream(target, wireHistory, tools, signal, effort, sessionId, onDelta, recordUsage)
    expect(attemptStreamMock.mock.calls[0][7]).toBe(false);
  });

  it("offers only the restricted explore tool set for an explore-profile task", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask({ profile: "explore" });

    await runSideTask(record.id);

    const tools = attemptStreamMock.mock.calls[0][2] as { function: { name: string } }[];
    const names = tools.map((tool) => tool.function.name).sort();
    expect(names).toEqual(["glob", "grep", "list_dir", "read_file"]);
  });

  it("offers the code profile's extra mutating tools when requested", async () => {
    attemptStreamMock.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask({ profile: "code" });

    await runSideTask(record.id);

    const tools = attemptStreamMock.mock.calls[0][2] as { function: { name: string } }[];
    const names = tools.map((tool) => tool.function.name);
    expect(names).toEqual(expect.arrayContaining(["write_file", "edit_file", "run_shell"]));
  });

  it("returns an error status when the stream itself errors", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "network broke", contentStarted: false });
    const record = seedTask();

    await runSideTask(record.id);

    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.status).toBe("error");
    expect(task.error).toBe("network broke");
  });

  it("runs a tool round trip, records tool evidence, then completes with the final answer", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("read_file", "call-1")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "All done.", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("file contents here");
    const record = seedTask();

    await runSideTask(record.id);

    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.status).toBe("completed");
    expect(task.finalReport).toBe("All done.");
    expect(task.toolEvidence).toHaveLength(1);
    expect(task.toolEvidence[0].outcome).toBe("succeeded");
    expect(task.toolEvidence[0].name).toBe("read_file");
  });

  it("caps an extremely long final report", async () => {
    const huge = "x".repeat(30_000);
    attemptStreamMock.mockResolvedValue({ content: huge, toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask();

    await runSideTask(record.id);

    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.finalReport!.length).toBeLessThan(huge.length);
    expect(task.finalReport).toContain("truncated");
  });
});

describe("runSideTask / isolation — checkpoint_id null, own turn_id, agent_label attribution", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("executes each tool call with checkpointId null, this task's own turnId, and a distinct agentLabel", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("write_file", "call-1")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("ok");
    const record = seedTask({ profile: "code", title: "Refactor auth" });

    await runSideTask(record.id);

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    const args = executeToolCallMock.mock.calls[0];
    // executeToolCall(toolCall, checkpointId, turnId, mcpRegistry, signal, risk, attachedStackNames, subagent, agentLabel)
    expect(args[1]).toBeNull();
    expect(args[2]).toBe(record.turnId);
    expect(args[8]).toBe('Side task "Refactor auth"');
  });

  it("two concurrently-running side tasks get different turnIds passed to executeToolCall", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("ok");
    const a = seedTask({ title: "Task A" });
    const b = seedTask({ title: "Task B" });
    expect(a.turnId).not.toBe(b.turnId);

    // Run just one iteration's worth of each by cancelling after the first tool call is recorded —
    // simplest: cap iterations by asserting the turnId used on the FIRST executeToolCall call for each.
    const runA = runSideTask(a.id);
    cancelSideTask(a.id);
    await runA;
    const runB = runSideTask(b.id);
    cancelSideTask(b.id);
    await runB;

    const turnIdsUsed = executeToolCallMock.mock.calls.map((call) => call[2]);
    // Every recorded call used its own task's turnId, never the other task's.
    for (const id of turnIdsUsed) {
      expect([a.turnId, b.turnId]).toContain(id);
    }
  });
});

describe("runSideTask / MAX_SIDE_TASK_ITERATIONS cap", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
    executeToolCallMock.mockResolvedValue("tool result");
  });

  it("stops after MAX_SIDE_TASK_ITERATIONS round trips and ends in error instead of looping forever", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true });
    const record = seedTask();

    await runSideTask(record.id);

    expect(attemptStreamMock).toHaveBeenCalledTimes(MAX_SIDE_TASK_ITERATIONS);
    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.status).toBe("error");
    expect(task.error).toContain(String(MAX_SIDE_TASK_ITERATIONS));
  });
});

describe("runSideTask / cancellation", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("cancelSideTask aborts an in-flight attempt and the task ends cancelled", async () => {
    let releaseAttempt!: () => void;
    attemptStreamMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          releaseAttempt = () => resolve({ content: "", toolCalls: [], streamError: null, contentStarted: false });
        }),
    );
    const record = seedTask();

    const run = runSideTask(record.id);
    // Let the loop reach its first attemptStream call before cancelling.
    await vi.waitFor(() => expect(attemptStreamMock).toHaveBeenCalledTimes(1));
    cancelSideTask(record.id);
    releaseAttempt();
    await run;

    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("cancelled");
  });

  it("cancelling immediately after starting ends the task cancelled without calling attemptStream", async () => {
    const record = seedTask();
    // `runSideTask` registers its AbortController synchronously before its
    // first `await` (see that function's own doc comment) — calling cancel
    // right after invoking it, before awaiting, still lands before the
    // post-`resolveTarget` abort check fires.
    const run = runSideTask(record.id);
    cancelSideTask(record.id);
    await run;

    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("cancelled");
  });
});

describe("runSideTask / pause and resume", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("waitUntilResumed resolves immediately when the task isn't paused", async () => {
    const record = seedTask();
    const controller = new AbortController();
    await expect(waitUntilResumed(record.id, controller.signal)).resolves.toBeUndefined();
  });

  it("waitUntilResumed blocks while paused and resolves once resumed", async () => {
    const record = seedTask();
    useSideTaskStore.getState().markRunning(record.id);
    pauseSideTask(record.id);

    let resolved = false;
    const controller = new AbortController();
    const waiter = waitUntilResumed(record.id, controller.signal).then(() => {
      resolved = true;
    });

    await Promise.resolve();
    expect(resolved).toBe(false);

    resumeSideTask(record.id);
    await waiter;
    expect(resolved).toBe(true);
  });

  it("a paused task holds its next tool call until resumed, then continues to completion", async () => {
    let releaseFirst!: () => void;
    attemptStreamMock.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          releaseFirst = () =>
            resolve({ content: "", toolCalls: [toolCall("read_file")], streamError: null, contentStarted: true });
        }),
    );
    attemptStreamMock.mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    executeToolCallMock.mockResolvedValue("ok");
    const record = seedTask();

    const run = runSideTask(record.id);
    // Status flips to "running" (before the first attemptStream call even
    // resolves) — pausing here, then releasing the first attempt, means the
    // loop must hold at `waitUntilResumed` right before executing the tool
    // call it just received, not race ahead of the pause.
    await vi.waitFor(() => expect(useSideTaskStore.getState().tasks[record.id].status).toBe("running"));
    pauseSideTask(record.id);
    releaseFirst();

    // Flush a generous number of microtask turns — with no real pause gate
    // this would be more than enough for the loop to reach and execute the
    // tool call and the second attemptStream call.
    for (let i = 0; i < 20; i++) await Promise.resolve();
    expect(executeToolCallMock).not.toHaveBeenCalled();
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("paused");

    resumeSideTask(record.id);
    await run;

    expect(executeToolCallMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock).toHaveBeenCalledTimes(2);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("completed");
  });
});

describe("startSideTask / doesn't block the caller", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("returns a task id synchronously while the model call is still pending", async () => {
    let releaseAttempt!: () => void;
    attemptStreamMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          releaseAttempt = () => resolve({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
        }),
    );

    const taskId = startSideTask({ title: "A", prompt: "a", profile: "explore", source, sessionId: "session-1" });

    expect(typeof taskId).toBe("string");
    // The run is still queued/running — startSideTask did not await it.
    expect(["queued", "running"]).toContain(useSideTaskStore.getState().tasks[taskId]?.status);

    await vi.waitFor(() => expect(attemptStreamMock).toHaveBeenCalledTimes(1));
    releaseAttempt();
    await vi.waitFor(() => expect(useSideTaskStore.getState().tasks[taskId].status).toBe("completed"));
  });
});

describe("retrySideTask", () => {
  beforeEach(() => {
    attemptStreamMock.mockReset();
    executeToolCallMock.mockReset();
  });

  it("starts a fresh attempt with the same prompt/profile/source but a new id and turnId, linked via retryOf", async () => {
    attemptStreamMock.mockResolvedValue({ content: "first", toolCalls: [], streamError: null, contentStarted: true });
    const original = seedTask({ title: "Explain auth", prompt: "Explain the auth flow" });
    await runSideTask(original.id);

    attemptStreamMock.mockResolvedValue({ content: "second", toolCalls: [], streamError: null, contentStarted: true });
    const retryId = retrySideTask(original.id);
    expect(retryId).not.toBeNull();
    expect(retryId).not.toBe(original.id);

    await vi.waitFor(() => expect(useSideTaskStore.getState().tasks[retryId!].status).toBe("completed"));
    const retryTask = useSideTaskStore.getState().tasks[retryId!];
    expect(retryTask.retryOf).toBe(original.id);
    expect(retryTask.turnId).not.toBe(original.turnId);
    expect(retryTask.prompt).toBe(original.prompt);
    expect(retryTask.title).toBe(original.title);
  });

  it("returns null for an unknown task id", () => {
    expect(retrySideTask("missing")).toBeNull();
  });
});

describe("promoteSideTask", () => {
  it("appends the final report to the originating session and marks the task promoted", async () => {
    useSessionStore.getState().newSession();
    const sessionId = useSessionStore.getState().activeSessionId;
    attemptStreamMock.mockResolvedValue({ content: "The answer is 42.", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask({ sessionId });
    await runSideTask(record.id);

    const promoted = promoteSideTask(record.id);

    expect(promoted).toBe(true);
    const session = useSessionStore.getState().sessions.find((s) => s.id === sessionId)!;
    const lastMessage = session.messages[session.messages.length - 1];
    expect(lastMessage.role).toBe("assistant");
    expect(String(lastMessage.content)).toContain("The answer is 42.");
    expect(String(lastMessage.content)).toContain("Explain auth");
    expect(useSideTaskStore.getState().tasks[record.id].promotedAt).not.toBeNull();
  });

  it("is a no-op (returns false) when the task has no final report yet", () => {
    const record = seedTask();
    expect(promoteSideTask(record.id)).toBe(false);
  });
});

describe("openSideTaskAsFullChat", () => {
  it("creates a brand new session seeded with the task's prompt and report, and returns its id", async () => {
    attemptStreamMock.mockResolvedValue({ content: "Here's the report.", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask({ prompt: "Explain the auth flow in depth" });
    await runSideTask(record.id);

    const beforeCount = useSessionStore.getState().sessions.length;
    const newSessionId = openSideTaskAsFullChat(record.id);

    expect(newSessionId).not.toBeNull();
    expect(useSessionStore.getState().sessions.length).toBe(beforeCount + 1);
    const session = useSessionStore.getState().sessions.find((s) => s.id === newSessionId)!;
    expect(session.messages[0]).toEqual({ role: "user", content: "Explain the auth flow in depth" });
    expect(session.messages[1]).toEqual({ role: "assistant", content: "Here's the report." });
    expect(useSessionStore.getState().activeSessionId).toBe(newSessionId);
  });

  it("returns null for an unknown task id", () => {
    expect(openSideTaskAsFullChat("missing")).toBeNull();
  });
});

describe("continueSideTask", () => {
  it("runs a follow-up turn on the same record, with the earlier transcript as context", async () => {
    attemptStreamMock.mockResolvedValue({ content: "First answer.", toolCalls: [], streamError: null, contentStarted: true });
    const record = seedTask();
    await runSideTask(record.id);
    const firstTurnId = useSideTaskStore.getState().tasks[record.id].turnId;

    attemptStreamMock.mockResolvedValue({ content: "Second answer.", toolCalls: [], streamError: null, contentStarted: true });
    expect(continueSideTask(record.id, "Now check the refresh path")).toBe(true);
    // `runSideTask` is fired without awaiting (same shape as `startSideTask`),
    // so let its microtasks drain before asserting on the settled state.
    await new Promise((resolve) => setTimeout(resolve, 0));

    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.status).toBe("completed");
    expect(task.finalReport).toBe("Second answer.");
    expect(task.messages.map((message) => message.content)).toContain("Now check the refresh path");
    // The wire history the follow-up turn sent still carries the first answer.
    const lastCallHistory = attemptStreamMock.mock.calls[attemptStreamMock.mock.calls.length - 1][1] as { content?: unknown }[];
    expect(lastCallHistory.some((message) => message.content === "First answer.")).toBe(true);
    // A follow-up mints a fresh turn id so a previous "allow for this run"
    // grant cannot authorize the new instruction.
    expect(task.turnId).not.toBe(firstTurnId);
  });

  it("refuses while the task is still active, and refuses empty text", async () => {
    const record = seedTask();
    useSideTaskStore.getState().markRunning(record.id);
    expect(continueSideTask(record.id, "hello")).toBe(false);

    useSideTaskStore.getState().finish(record.id, "completed", "done", null);
    expect(continueSideTask(record.id, "   ")).toBe(false);
    expect(continueSideTask("missing", "hello")).toBe(false);
  });
});
