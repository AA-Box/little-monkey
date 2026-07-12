import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

// `executeToolCall`'s `task` branch delegates to `subagent.ts`'s
// `runSubagentTask` — mocked out entirely here so these tests can pin the
// DELEGATION contract (which params get threaded through, error handling)
// without also having to drive a real child model-streaming loop; the child
// loop itself is covered by `subagent.test.ts`.
const runSubagentTaskMock = vi.fn();
vi.mock("./subagent", () => ({ runSubagentTask: (...args: unknown[]) => runSubagentTaskMock(...args) }));

// `attemptStream`'s `recordUsage` tests below need a controllable stream —
// mocked here rather than hitting a real provider.
const streamProviderChatMock = vi.fn();
vi.mock("./providerClient", () => ({ streamProviderChat: (...args: unknown[]) => streamProviderChatMock(...args) }));

import {
  attemptStream,
  executeToolCall,
  PRESENT_PLAN_RESULT,
  stringifyToolError,
  type ResolvedTarget,
  type RiskAnnotationContext,
  type SubagentContext,
} from "./turnEngine";
import type { RiskClassification } from "./riskJudge";
import type { McpToolRegistry } from "./mcpTools";
import type { StreamEvent, ToolCall } from "./llamaClient";
import { useUsageStore } from "../store/usageStore";

const emptyMcpRegistry: McpToolRegistry = new Map();

function call(name: string, args: Record<string, unknown> = {}): ToolCall {
  return { id: `call-${name}`, type: "function", function: { name, arguments: JSON.stringify(args) } };
}

describe("executeToolCall / present_plan", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  // The critical security-adjacent invariant this tool depends on: it has no
  // `tool_present_plan` Rust command (see `tools.ts`'s `PRESENT_PLAN_TOOL` doc
  // comment), so `invoke` must never be called for it — not even indirectly,
  // e.g. via a typo'd dispatch path that would otherwise surface as a
  // confusing "command not found" IPC error instead of the clean fixed result
  // the model is meant to see.
  it("never reaches invoke for a present_plan tool call", async () => {
    const result = await executeToolCall(call("present_plan", { title: "Refactor auth", plan: "1. Do X\n2. Do Y" }), null, "turn-1", emptyMcpRegistry);

    expect(invokeMock).not.toHaveBeenCalled();
    expect(result).toBe(PRESENT_PLAN_RESULT);
  });

  it("never reaches invoke for present_plan even when a checkpoint/signal are supplied", async () => {
    const controller = new AbortController();
    const result = await executeToolCall(
      call("present_plan", { title: "T", plan: "P" }),
      "checkpoint-123",
      "turn-1",
      emptyMcpRegistry,
      controller.signal
    );

    expect(invokeMock).not.toHaveBeenCalled();
    expect(result).toBe(PRESENT_PLAN_RESULT);
  });

  it("still dispatches to invoke for every other tool name (present_plan is the only frontend-only exception)", async () => {
    invokeMock.mockResolvedValue("ok");
    await executeToolCall(call("read_file", { path: "a.txt" }), null, "turn-1", emptyMcpRegistry);
    expect(invokeMock).toHaveBeenCalledWith("tool_read_file", expect.objectContaining({ path: "a.txt" }));
  });
});

// `allowed_stack_names` is the server-side enforcement point for
// `search_docs`'s session scoping (see `stacks.rs`'s
// `resolve_search_stack_ids` doc comment for the privacy gap this closes):
// it must always be injected with THIS turn's actual attached-stack names,
// regardless of what (if anything) the model itself passed.
describe("executeToolCall / search_docs stack scoping", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue([]);
  });

  it("injects the session's attached stack names as allowed_stack_names", async () => {
    await executeToolCall(
      call("search_docs", { query: "budget planning" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      ["Work Docs", "Wiki"]
    );

    const [command, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(command).toBe("tool_search_docs");
    expect(sentArgs.allowed_stack_names).toEqual(["Work Docs", "Wiki"]);
  });

  it("defaults allowed_stack_names to an empty array when no attached names are supplied", async () => {
    await executeToolCall(call("search_docs", { query: "anything" }), null, "turn-1", emptyMcpRegistry);

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.allowed_stack_names).toEqual([]);
  });

  it("overwrites a model-supplied allowed_stack_names — the model can never widen its own scope", async () => {
    await executeToolCall(
      call("search_docs", { query: "q", allowed_stack_names: ["Diary"] }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      ["Work Docs"]
    );

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.allowed_stack_names).toEqual(["Work Docs"]);
  });
});

// IPC-level tests pinning the risk_level/risk_reason scrub-then-overwrite
// invariant — mirrors the spirit of tools.rs's
// `edit_file_ipc_accepts_snake_case_argument_keys` test, but on the frontend
// side of the boundary: these keys are frontend-owned exactly like
// `checkpoint_id`/`turn_id`, and this is what actually enforces that a
// model can never smuggle its own risk rating through, since it's the last
// place before the value crosses into Rust via `invoke`.
describe("executeToolCall / risk_level and risk_reason scrubbing", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue("ok");
  });

  it("strips a model-supplied risk_level/risk_reason when risk annotations are disabled", async () => {
    const toolCall = call("write_file", {
      path: "a.txt",
      content: "x",
      risk_level: "low",
      risk_reason: "trust me, totally safe",
    });

    await executeToolCall(toolCall, null, "turn-1", emptyMcpRegistry, undefined, {
      enabled: false,
      cache: new Map(),
      classify: vi.fn(),
    });

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.risk_level).toBeUndefined();
    expect(sentArgs.risk_reason).toBeUndefined();
  });

  it("strips a model-supplied risk_level/risk_reason even with no risk context supplied at all", async () => {
    const toolCall = call("run_shell", { command: "ls", risk_level: "low", risk_reason: "harmless" });

    await executeToolCall(toolCall, null, "turn-1", emptyMcpRegistry);

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.risk_level).toBeUndefined();
    expect(sentArgs.risk_reason).toBeUndefined();
  });

  it("overwrites a model-supplied risk_level/risk_reason with the judge's own result when annotations are enabled — the model's values never survive", async () => {
    const toolCall = call("write_file", {
      path: "a.txt",
      content: "x",
      risk_level: "low",
      risk_reason: "trust me, totally safe",
    });

    // `executeToolCall` mutates the same `args` object in place after
    // `classify` resolves (to inject the judge's result) — snapshot what
    // `classify` was actually called with (a clone) rather than reading
    // `classify.mock.calls` afterward, which would observe the post-mutation
    // object instead of what the judge was actually shown.
    let snapshotAtClassifyTime: Record<string, unknown> | null = null;
    const classify = vi.fn(async (_tool: string, seenArgs: Record<string, unknown>): Promise<RiskClassification> => {
      snapshotAtClassifyTime = { ...seenArgs };
      return { level: "high", reason: "judge says risky" };
    });
    const risk: RiskAnnotationContext = { enabled: true, cache: new Map(), classify };

    await executeToolCall(toolCall, null, "turn-1", emptyMcpRegistry, undefined, risk);

    // The judge was called with the model's args already stripped of any
    // risk_level/risk_reason it tried to supply.
    expect(snapshotAtClassifyTime).not.toBeNull();
    expect(snapshotAtClassifyTime!.risk_level).toBeUndefined();
    expect(snapshotAtClassifyTime!.risk_reason).toBeUndefined();

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.risk_level).toBe("high");
    expect(sentArgs.risk_reason).toBe("judge says risky");
  });

  it("classifies write_file, edit_file, and run_shell, but not read-only tools", async () => {
    const classify = vi.fn(async (): Promise<RiskClassification | null> => ({ level: "medium", reason: "r" }));
    const risk: RiskAnnotationContext = { enabled: true, cache: new Map(), classify };

    await executeToolCall(call("write_file", { path: "a.txt", content: "x" }), null, "turn-1", emptyMcpRegistry, undefined, risk);
    await executeToolCall(call("edit_file", { path: "b.txt", old_string: "a", new_string: "b" }), null, "turn-1", emptyMcpRegistry, undefined, risk);
    await executeToolCall(call("run_shell", { command: "ls" }), null, "turn-1", emptyMcpRegistry, undefined, risk);
    await executeToolCall(call("read_file", { path: "c.txt" }), null, "turn-1", emptyMcpRegistry, undefined, risk);

    expect(classify).toHaveBeenCalledTimes(3);
  });

  it("never calls classify when annotations are disabled, for any tool", async () => {
    const classify = vi.fn();
    const risk: RiskAnnotationContext = { enabled: false, cache: new Map(), classify };

    await executeToolCall(call("write_file", { path: "a.txt", content: "x" }), null, "turn-1", emptyMcpRegistry, undefined, risk);
    await executeToolCall(call("run_shell", { command: "ls" }), null, "turn-1", emptyMcpRegistry, undefined, risk);

    expect(classify).not.toHaveBeenCalled();
  });

  it("reuses a cached classification for an identical (tool, args) pair instead of calling classify again", async () => {
    const classify = vi.fn(async (): Promise<RiskClassification> => ({ level: "low", reason: "cached" }));
    const cache = new Map<string, RiskClassification | null>();
    const risk: RiskAnnotationContext = { enabled: true, cache, classify };

    const args = { path: "a.txt", content: "same" };
    await executeToolCall(call("write_file", args), null, "turn-1", emptyMcpRegistry, undefined, risk);
    await executeToolCall(call("write_file", args), null, "turn-1", emptyMcpRegistry, undefined, risk);

    expect(classify).toHaveBeenCalledTimes(1);
    const [, secondCallArgs] = invokeMock.mock.calls[1] as [string, Record<string, unknown>];
    expect(secondCallArgs.risk_level).toBe("low");
    expect(secondCallArgs.risk_reason).toBe("cached");
  });

  it("injects no risk_level/risk_reason at all when the judge fails closed (returns null)", async () => {
    const classify = vi.fn(async (): Promise<RiskClassification | null> => null);
    const risk: RiskAnnotationContext = { enabled: true, cache: new Map(), classify };

    await executeToolCall(call("write_file", { path: "a.txt", content: "x" }), null, "turn-1", emptyMcpRegistry, undefined, risk);

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.risk_level).toBeUndefined();
    expect(sentArgs.risk_reason).toBeUndefined();
  });
});

// `task` is another frontend-only tool, same treatment as `present_plan`
// above — it has no `tool_task` Rust command, so `invoke` must never be
// called for it either. These tests also pin the delegation contract that
// makes subagents safe: the PARENT's checkpoint id passed straight through,
// but a brand-new (never the parent's) turn id for the child, and the
// transcript-validity invariant (a `task` call always gets SOME string
// result, even when `runSubagentTask` itself throws).
describe("executeToolCall / task delegation", () => {
  const fakeTarget: ResolvedTarget = { kind: "local", baseUrl: "http://localhost:8090" };

  beforeEach(() => {
    invokeMock.mockReset();
    runSubagentTaskMock.mockReset();
  });

  it("never reaches invoke for a task tool call", async () => {
    runSubagentTaskMock.mockResolvedValue("a report");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };
    const result = await executeToolCall(
      call("task", { description: "find X", prompt: "find every caller of X", profile: "explore" }),
      "checkpoint-1",
      "parent-turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      subagent
    );

    expect(invokeMock).not.toHaveBeenCalled();
    expect(result).toBe("a report");
  });

  it("returns a tool-error result without calling runSubagentTask when no subagent context is configured", async () => {
    const result = await executeToolCall(call("task", { description: "d", prompt: "p", profile: "explore" }), null, "turn-1", emptyMcpRegistry);

    expect(runSubagentTaskMock).not.toHaveBeenCalled();
    expect(JSON.parse(result)).toHaveProperty("error");
  });

  it("passes the PARENT's checkpoint id through unchanged, but a brand-new turn id for the child (never the parent's)", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget, effort: "high" };

    await executeToolCall(
      call("task", { description: "find X", prompt: "find every caller of X", profile: "explore" }),
      "parent-checkpoint-123",
      "parent-turn-abc",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      subagent
    );

    expect(runSubagentTaskMock).toHaveBeenCalledTimes(1);
    const params = runSubagentTaskMock.mock.calls[0][0];
    expect(params.parentCheckpointId).toBe("parent-checkpoint-123");
    expect(params.taskId).not.toBe("parent-turn-abc");
    expect(typeof params.taskId).toBe("string");
    expect(params.taskId.length).toBeGreaterThan(0);
    expect(params.sessionId).toBe("session-1");
    expect(params.target).toBe(fakeTarget);
    expect(params.effort).toBe("high");
    expect(params.description).toBe("find X");
    expect(params.prompt).toBe("find every caller of X");
    expect(params.profile).toBe("explore");
  });

  it("gives each task call its own distinct turn id, even across two calls in the same tool round trip", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };

    await executeToolCall(call("task", { description: "a", prompt: "p", profile: "explore" }), null, "parent-turn", emptyMcpRegistry, undefined, undefined, undefined, subagent);
    await executeToolCall(call("task", { description: "b", prompt: "p", profile: "explore" }), null, "parent-turn", emptyMcpRegistry, undefined, undefined, undefined, subagent);

    const firstTaskId = runSubagentTaskMock.mock.calls[0][0].taskId;
    const secondTaskId = runSubagentTaskMock.mock.calls[1][0].taskId;
    expect(firstTaskId).not.toBe(secondTaskId);
  });

  it("defaults a missing/invalid profile to 'explore' rather than passing the model's raw value through", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };

    await executeToolCall(call("task", { description: "d", prompt: "p" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, subagent);

    expect(runSubagentTaskMock.mock.calls[0][0].profile).toBe("explore");
  });

  // The transcript-validity invariant this whole feature depends on: a
  // `task` call must ALWAYS get a tool result, even when the child loop
  // blows up unexpectedly — `executeToolCall`'s own try/catch around the
  // `task` branch must swallow it, not let it propagate to the caller
  // (`runAgentTurnBody`'s tool-calling loop), which would otherwise crash
  // the whole turn on an exception from deep inside a subagent.
  it("never propagates an exception thrown by runSubagentTask — returns a stringifyToolError-shaped result instead", async () => {
    runSubagentTaskMock.mockRejectedValue(new Error("child loop exploded"));
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };

    const result = await executeToolCall(
      call("task", { description: "d", prompt: "p", profile: "explore" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      subagent
    );

    expect(result).toBe(stringifyToolError(new Error("child loop exploded")));
  });
});

// `recordUsage` (attemptStream's newest parameter) is what stops a
// subagent's own model calls from clobbering the PARENT session's
// context-usage ring in `useUsageStore` — see `subagent.ts`'s
// `runSubagentTask`, the one caller that passes `false`. These tests pin the
// parameter's behavior directly against a controllable fake stream.
describe("attemptStream / recordUsage", () => {
  const fakeTarget: ResolvedTarget = { kind: "provider", providerId: "openai", model: "gpt-test" };

  async function* fakeUsageStream(): AsyncGenerator<StreamEvent> {
    yield { type: "usage", usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } };
    yield { type: "done" };
  }

  beforeEach(() => {
    streamProviderChatMock.mockReset();
    streamProviderChatMock.mockImplementation(() => fakeUsageStream());
    useUsageStore.setState({ usageBySession: {}, contextLimit: null });
  });

  it("records usage into useUsageStore by default (recordUsage defaults to true — every pre-existing caller is unaffected)", async () => {
    await attemptStream(fakeTarget, [], [], undefined, undefined, "session-1");

    expect(useUsageStore.getState().usageBySession["session-1"]).toEqual({
      promptTokens: 10,
      completionTokens: 5,
      totalTokens: 15,
    });
  });

  it("does not record usage into useUsageStore when recordUsage is false — a subagent child call must never clobber the parent session's usage ring", async () => {
    await attemptStream(fakeTarget, [], [], undefined, undefined, "session-1", undefined, false);

    expect(useUsageStore.getState().usageBySession["session-1"]).toBeUndefined();
  });
});
