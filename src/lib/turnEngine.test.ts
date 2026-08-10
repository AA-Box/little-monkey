import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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
  CANCELLED_TOOL_RESULT,
  executeToolCall,
  isBlockedInPlanMode,
  isToolCallAllowed,
  PRESENT_PLAN_RESULT,
  stringifyToolError,
  type ResolvedTarget,
  type RiskAnnotationContext,
  type SkillToolContext,
  type SubagentContext,
} from "./turnEngine";
import type { RiskClassification } from "./riskJudge";
import type { McpToolRegistry } from "./mcpTools";
import type { StreamEvent, ToolCall, ToolDef } from "./llamaClient";
import type { SlashSkill } from "./skills";
import { useUsageStore } from "../store/usageStore";
import {
  DEFAULT_COST_BUDGET_POLICY,
  useCostControlStore,
} from "../store/costControlStore";
import { usePrivacyFirewallStore } from "../store/privacyFirewallStore";
import { useWorkspaceStore } from "../store/workspaceStore";
import { useSessionStore } from "../store/sessionStore";
import { providerModelTargetKey } from "./modelTargets";
import { useUserHooksStore } from "../store/userHooksStore";
import { usePermissionStore } from "../store/permissionStore";

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

  it("passes the originating tool_call's own id as toolCallId — the subagentStore/ChatSession.subagentRuns correlation key, distinct from taskId", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };
    const toolCall = call("task", { description: "find X", prompt: "p", profile: "explore" });

    await executeToolCall(toolCall, null, "parent-turn", emptyMcpRegistry, undefined, undefined, undefined, subagent);

    const params = runSubagentTaskMock.mock.calls[0][0];
    expect(params.toolCallId).toBe(toolCall.id);
    expect(params.toolCallId).not.toBe(params.taskId);
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

  // Review findings: a code-profile subagent's mutations must get the same
  // risk classification the parent's own calls would, and must be able to
  // report both successes and failures back into the parent's mutation
  // tracking — all threaded straight through to `runSubagentTask`'s params.
  it("threads SubagentContext mutation callbacks and risk through to runSubagentTask unchanged", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const risk: RiskAnnotationContext = { enabled: true, cache: new Map(), classify: vi.fn() };
    const onMutatedPath = vi.fn();
    const onMutationFailure = vi.fn();
    const subagent: SubagentContext = {
      sessionId: "session-1",
      target: fakeTarget,
      risk,
      onMutatedPath,
      onMutationFailure,
    };

    await executeToolCall(
      call("task", { description: "d", prompt: "p", profile: "code" }),
      "checkpoint-1",
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      subagent
    );

    const params = runSubagentTaskMock.mock.calls[0][0];
    expect(params.risk).toBe(risk);
    expect(params.onMutatedPath).toBe(onMutatedPath);
    expect(params.onMutationFailure).toBe(onMutationFailure);
  });

  it("passes optional risk and mutation callbacks as undefined when omitted", async () => {
    runSubagentTaskMock.mockResolvedValue("done");
    const subagent: SubagentContext = { sessionId: "session-1", target: fakeTarget };

    await executeToolCall(call("task", { description: "d", prompt: "p", profile: "explore" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, subagent);

    const params = runSubagentTaskMock.mock.calls[0][0];
    expect(params.risk).toBeUndefined();
    expect(params.onMutatedPath).toBeUndefined();
    expect(params.onMutationFailure).toBeUndefined();
  });
});

// `skill` is a third frontend-only tool (see `tools.ts`'s `SKILL_INVOKE_TOOL`
// doc comment) — same "invoke must never be called" invariant as
// `present_plan`/`task` above, plus the auto-invocation-specific contract:
// unknown/duplicate/over-cap commands are rejected with a tool error rather
// than throwing, and a successful call both returns the skill's instructions
// AND records the command in `invokedCommands` so a later call in the same
// turn sees it as already-invoked.
describe("executeToolCall / skill invocation", () => {
  function fakeSkill(command: string, overrides: Partial<SlashSkill> = {}): SlashSkill {
    return {
      id: command,
      source: "native",
      command,
      name: command,
      description: `Handles ${command}`,
      instructions: `Do ${command} carefully.`,
      version: "1.0.0",
      contentSha256: "a".repeat(64),
      permissions: [],
      ...overrides,
    };
  }

  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("never reaches invoke for a skill tool call", async () => {
    const context: SkillToolContext = {
      availableSkills: [fakeSkill("review")],
      invokedCommands: new Set(),
      maxSkillsPerTurn: 5,
    };
    const result = await executeToolCall(
      call("skill", { command: "review" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      context
    );

    expect(invokeMock).not.toHaveBeenCalled();
    expect(result).toContain("Do review carefully.");
    expect(context.invokedCommands.has("review")).toBe(true);
  });

  it("includes allowed-tools and bundled-file listings in the result", async () => {
    const context: SkillToolContext = {
      availableSkills: [fakeSkill("review", { allowedTools: ["read_file", "grep"], resourceFiles: ["references/info.md"] })],
      invokedCommands: new Set(),
      maxSkillsPerTurn: 5,
    };
    const result = await executeToolCall(call("skill", { command: "review" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, undefined, context);

    expect(result).toContain("Allowed tools while active: read_file, grep");
    expect(result).toContain("Bundled files (read via read_skill_resource): references/info.md");
  });

  it("accepts a leading slash in the command argument", async () => {
    const context: SkillToolContext = { availableSkills: [fakeSkill("review")], invokedCommands: new Set(), maxSkillsPerTurn: 5 };
    const result = await executeToolCall(call("skill", { command: "/review" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, undefined, context);
    expect(result).toContain("Do review carefully.");
  });

  it("returns a tool error without calling invoke when no skill context is configured", async () => {
    const result = await executeToolCall(call("skill", { command: "review" }), null, "turn-1", emptyMcpRegistry);
    expect(invokeMock).not.toHaveBeenCalled();
    expect(JSON.parse(result)).toHaveProperty("error");
  });

  it("rejects an unknown command", async () => {
    const context: SkillToolContext = { availableSkills: [fakeSkill("review")], invokedCommands: new Set(), maxSkillsPerTurn: 5 };
    const result = await executeToolCall(call("skill", { command: "does-not-exist" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, undefined, context);
    expect(JSON.parse(result).error).toMatch(/No enabled skill/);
    expect(context.invokedCommands.size).toBe(0);
  });

  it("rejects a command already invoked this turn (explicit or previously model-invoked)", async () => {
    const context: SkillToolContext = {
      availableSkills: [fakeSkill("review")],
      invokedCommands: new Set(["review"]),
      maxSkillsPerTurn: 5,
    };
    const result = await executeToolCall(call("skill", { command: "review" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, undefined, context);
    expect(JSON.parse(result).error).toMatch(/already invoked/);
  });

  it("rejects once the per-turn skill cap is reached", async () => {
    const context: SkillToolContext = {
      availableSkills: [fakeSkill("review"), fakeSkill("verify")],
      invokedCommands: new Set(["review"]),
      maxSkillsPerTurn: 1,
    };
    const result = await executeToolCall(call("skill", { command: "verify" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, undefined, context);
    expect(JSON.parse(result).error).toMatch(/at most 1 skill/);
  });
});

// `read_skill_resource` has a real `tool_read_skill_resource` Rust command
// (unlike `skill` above), but its own tool description promises it "only
// works for a skill that has already been invoked this turn" — these tests
// pin that this is actually enforced here, before the generic `invoke`
// dispatch, rather than just asserted in prose the model might not honor.
describe("executeToolCall / read_skill_resource invocation gate", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue("resource contents");
  });

  it("rejects when the named skill has not been invoked this turn", async () => {
    const context: SkillToolContext = {
      availableSkills: [{ id: "review", source: "native", command: "review", name: "review", description: "d", instructions: "i", version: "1.0.0", contentSha256: "a".repeat(64), permissions: [] }],
      invokedCommands: new Set(),
      maxSkillsPerTurn: 5,
    };
    const result = await executeToolCall(
      call("read_skill_resource", { command: "review", path: "references/info.md" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      context
    );
    expect(invokeMock).not.toHaveBeenCalled();
    expect(JSON.parse(result).error).toMatch(/has not been invoked this turn/);
  });

  it("allows the call once the skill was invoked (explicitly or via the skill tool) this turn", async () => {
    const context: SkillToolContext = {
      availableSkills: [{ id: "review", source: "native", command: "review", name: "review", description: "d", instructions: "i", version: "1.0.0", contentSha256: "a".repeat(64), permissions: [] }],
      invokedCommands: new Set(["review"]),
      maxSkillsPerTurn: 5,
    };
    const result = await executeToolCall(
      call("read_skill_resource", { command: "review", path: "references/info.md" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      context
    );
    expect(invokeMock).toHaveBeenCalledWith("tool_read_skill_resource", { command: "review", path: "references/info.md" });
    expect(result).toBe("resource contents");
  });

  it("returns a tool error without calling invoke when no skill context is configured", async () => {
    const result = await executeToolCall(call("read_skill_resource", { command: "review", path: "references/info.md" }), null, "turn-1", emptyMcpRegistry);
    expect(invokeMock).not.toHaveBeenCalled();
    expect(JSON.parse(result)).toHaveProperty("error");
  });
});

// slice 3: `code`-profile subagents mean `write_file`/`edit_file`/`run_shell`
// can now be dispatched from INSIDE `subagent.ts`'s `runSubagentTask`, not
// just the parent turn — these tests pin the two invariants that make that
// safe: (1) `agent_label` (subagent attribution, purely cosmetic — forwarded
// to Rust as its own field, see `permissions.rs`'s
// `PermissionRequestPayload.agent_label`) is never model-suppliable,
// mirroring the `risk_level`/`risk_reason` scrub tests above; (2) a mutating
// call routed
// through `executeToolCall` with the PARENT's checkpoint id but the CHILD's
// own turn id (exactly how `subagent.ts` calls it) forwards both correctly
// and distinctly — the crux pairing the design doc calls out.
describe("executeToolCall / agent_label scrubbing (subagent attribution, slice 3)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue("ok");
  });

  it("strips a model-supplied agent_label before dispatch — the model can never forge its own subagent attribution", async () => {
    await executeToolCall(call("write_file", { path: "a.txt", content: "x", agent_label: "totally not a subagent" }), null, "turn-1", emptyMcpRegistry);

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.agent_label).toBeUndefined();
  });

  it("injects the caller-supplied agentLabel, overwriting whatever the model tried to pass", async () => {
    await executeToolCall(
      call("write_file", { path: "a.txt", content: "x", agent_label: "model-forged label" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      "refactor auth"
    );

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.agent_label).toBe("refactor auth");
  });

  it("never adds an agent_label key at all for an ordinary parent-turn call (no agentLabel supplied)", async () => {
    await executeToolCall(call("write_file", { path: "a.txt", content: "x" }), null, "turn-1", emptyMcpRegistry);

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.agent_label).toBeUndefined();
  });

  it("only injects agent_label for write_file/edit_file/run_shell, never for a read-only tool", async () => {
    await executeToolCall(call("read_file", { path: "a.txt" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, "some label");

    const [, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(sentArgs.agent_label).toBeUndefined();
  });

  it("applies to edit_file and run_shell too, not just write_file", async () => {
    await executeToolCall(
      call("edit_file", { path: "a.txt", old_string: "a", new_string: "b" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      "edit label"
    );
    await executeToolCall(call("run_shell", { command: "ls" }), null, "turn-1", emptyMcpRegistry, undefined, undefined, undefined, undefined, "shell label");

    const [, editArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    const [, shellArgs] = invokeMock.mock.calls[1] as [string, Record<string, unknown>];
    expect(editArgs.agent_label).toBe("edit label");
    expect(shellArgs.agent_label).toBe("shell label");
  });
});

// The other half of "get this pairing exactly right" (design doc, slice 3):
// a `code`-profile subagent's own mutating tool calls must carry the
// PARENT's checkpoint id (so writes land in the parent turn's checkpoint,
// revertable via the existing CheckpointRow) but a DISTINCT, child-owned
// turn id (so Rust's per-turn `tool_cancel`/permission-`pending` maps scope
// cancellation and prompts to the subagent, not the parent's own in-flight
// call). `subagent.ts`'s `runSubagentTask` calls `executeToolCall` with
// exactly `(toolCall, parentCheckpointId, taskId, ...)` — this test exercises
// `executeToolCall` directly with that same argument shape.
describe("executeToolCall / checkpoint_id + turn_id pairing for a code-profile subagent's tool call", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue("Wrote 2 bytes to a.txt");
  });

  it("forwards the PARENT's checkpoint id but the CHILD's own (distinct) turn id for a write_file call", async () => {
    const parentCheckpointId = "parent-checkpoint-123";
    const parentTurnId = "parent-turn-abc";
    const childTurnId = "child-turn-xyz";

    await executeToolCall(call("write_file", { path: "a.txt", content: "hi" }), parentCheckpointId, childTurnId, emptyMcpRegistry);

    const [command, sentArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(command).toBe("tool_write_file");
    expect(sentArgs.checkpoint_id).toBe(parentCheckpointId);
    expect(sentArgs.turn_id).toBe(childTurnId);
    expect(sentArgs.turn_id).not.toBe(parentTurnId);
  });

  it("same pairing holds for edit_file and run_shell", async () => {
    const parentCheckpointId = "parent-checkpoint-456";
    const childTurnId = "child-turn-789";

    await executeToolCall(call("edit_file", { path: "a.txt", old_string: "a", new_string: "b" }), parentCheckpointId, childTurnId, emptyMcpRegistry);
    await executeToolCall(call("run_shell", { command: "ls" }), parentCheckpointId, childTurnId, emptyMcpRegistry);

    const [, editArgs] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    const [, shellArgs] = invokeMock.mock.calls[1] as [string, Record<string, unknown>];
    expect(editArgs.checkpoint_id).toBe(parentCheckpointId);
    expect(editArgs.turn_id).toBe(childTurnId);
    expect(shellArgs.checkpoint_id).toBe(parentCheckpointId);
    expect(shellArgs.turn_id).toBe(childTurnId);
  });
});

// Stop-button scoping for a subagent tree (design doc: "Stop in the parent
// pane cancels the whole tree, Stop in the OTHER split pane touches
// nothing"). `subagent.ts` passes the PARENT's own `AbortSignal` straight
// through to `executeToolCall` as `signal`, but with the CHILD's own turn id
// — so on abort, `tools_cancel_running` must be called with the CHILD's turn
// id, and a different turn's own (unrelated) AbortController aborting must
// never trigger it at all.
describe("executeToolCall / Stop-button cancellation scoping (subagent tree)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("cancels via tools_cancel_running scoped to the CHILD's own turn id when the (parent's) signal aborts mid-invocation", async () => {
    let rejectInvoke: (reason?: unknown) => void = () => {};
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "tool_run_shell") {
        return new Promise((_resolve, reject) => {
          rejectInvoke = reject;
        });
      }
      return Promise.resolve(undefined); // tools_cancel_running
    });

    const parentSignalStandIn = new AbortController();
    const childTurnId = "child-turn-1";
    const resultPromise = executeToolCall(call("run_shell", { command: "sleep 10" }), null, childTurnId, emptyMcpRegistry, parentSignalStandIn.signal);

    parentSignalStandIn.abort();
    const result = await resultPromise;
    rejectInvoke(new Error("aborted (unused)"));

    expect(result).toBe(CANCELLED_TOOL_RESULT);
    expect(invokeMock).toHaveBeenCalledWith("tools_cancel_running", { turnId: childTurnId });
  });

  it("a DIFFERENT (unrelated) turn's own AbortController aborting never cancels this call or touches its turn id", async () => {
    invokeMock.mockImplementation((cmd: string) => (cmd === "tool_run_shell" ? Promise.resolve({ stdout: "ok" }) : Promise.resolve(undefined)));

    const thisCallsOwnController = new AbortController(); // never aborted — "the parent pane's own turn"
    const otherPaneController = new AbortController(); // "the other split pane's turn"
    otherPaneController.abort();

    const result = await executeToolCall(call("run_shell", { command: "echo hi" }), null, "this-turn", emptyMcpRegistry, thisCallsOwnController.signal);

    expect(JSON.parse(result)).toEqual({ stdout: "ok" });
    expect(invokeMock).not.toHaveBeenCalledWith("tools_cancel_running", expect.anything());
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
    useCostControlStore.setState({
      policy: { ...DEFAULT_COST_BUDGET_POLICY },
      rates: {},
      entries: [],
    });
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

  // Slice 4 (per-subagent token usage in SubagentRow): `recordUsage: false`
  // must only gate the `useUsageStore` write above, never the CALLER's own
  // visibility into its attempt's usage — `subagent.ts`'s `runSubagentTask`
  // reads this field to accumulate a per-subagent total.
  it("still returns the attempt's own usage on the result even when recordUsage is false", async () => {
    const result = await attemptStream(fakeTarget, [], [], undefined, undefined, "session-1", undefined, false);

    expect(result.usage).toEqual({ promptTokens: 10, completionTokens: 5, totalTokens: 15 });
  });

  it("returns usage undefined when no usage event ever arrives", async () => {
    streamProviderChatMock.mockImplementation(async function* (): AsyncGenerator<StreamEvent> {
      yield { type: "done" };
    });

    const result = await attemptStream(fakeTarget, [], [], undefined, undefined, "session-1");

    expect(result.usage).toBeUndefined();
  });

  it("attributes priced usage to the exact provider model, session, and durable run", async () => {
    const targetKey = providerModelTargetKey("openai", "gpt-test");
    useCostControlStore.setState({
      rates: {
        [targetKey]: {
          inputPerMillionUsd: 2,
          outputPerMillionUsd: 8,
        },
      },
    });

    await attemptStream(
      fakeTarget,
      [],
      [],
      undefined,
      undefined,
      "session-cost",
      undefined,
      false,
      undefined,
      "run-cost",
    );

    expect(useCostControlStore.getState().entries).toEqual([
      expect.objectContaining({
        targetKey,
        targetLabel: "openai · gpt-test",
        sessionId: "session-cost",
        runId: "run-cost",
        usage: { promptTokens: 10, completionTokens: 5, totalTokens: 15 },
        costUsd: 0.00006,
      }),
    ]);
  });

  // K25: the recorded entry has to say WHERE to charge the call, and the two
  // places are genuinely different — the folder open right now (which is what
  // the process ledger stamps on the processes this turn spawns) and the
  // folder the conversation belongs to.
  it("attributes a call to the open workspace and to the session's own project folder", async () => {
    useWorkspaceStore.setState({
      roots: [{ id: "root-1", path: "/work/current", label: "current", is_primary: true }],
    });
    useSessionStore.setState({
      sessions: [{ id: "session-cost", workspacePath: "/work/older" }],
    } as unknown as Parameters<typeof useSessionStore.setState>[0]);

    await attemptStream(fakeTarget, [], [], undefined, undefined, "session-cost");

    expect(useCostControlStore.getState().entries[0]).toMatchObject({
      workspacePath: "/work/current",
      projectPath: "/work/older",
    });
  });

  it("records no attribution rather than a guessed one when there is no folder or session", async () => {
    useWorkspaceStore.setState({ roots: [] });
    useSessionStore.setState({ sessions: [] } as unknown as Parameters<
      typeof useSessionStore.setState
    >[0]);

    await attemptStream(fakeTarget, [], [], undefined, undefined, "session-unknown");

    expect(useCostControlStore.getState().entries[0]).toMatchObject({
      workspacePath: null,
      projectPath: null,
    });
  });

  it("pauses a provider request before transport when a hard budget is reached", async () => {
    useCostControlStore.setState({
      policy: {
        enabled: true,
        dailyBudgetUsd: 1,
        monthlyBudgetUsd: null,
        warningPercents: [0.8],
        enforcement: "pause",
      },
      entries: [
        {
          id: "spent",
          occurredAtMs: Date.now(),
          targetKey: providerModelTargetKey("openai", "gpt-test"),
          targetLabel: "openai · gpt-test",
          sessionId: "prior",
          runId: null,
          usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
          costUsd: 1,
        },
      ],
    });

    const result = await attemptStream(
      fakeTarget,
      [],
      [],
      undefined,
      undefined,
      "session-1",
    );

    expect(result.streamError).toContain("Cloud request paused");
    expect(result.contentStarted).toBe(false);
    expect(streamProviderChatMock).not.toHaveBeenCalled();
  });
});

// Effort used to be forwarded for Anthropic only; it now travels for EVERY
// provider target, because the Rust proxy owns the per-provider wire
// mapping/omission (verbatim output_config.effort for Anthropic, clamped
// reasoning_effort for OpenAI/Gemini/OpenRouter, dropped entirely for custom
// endpoints — see providers.rs::build_chat_request and its tests).
describe("attemptStream / effort forwarding", () => {
  async function* fakeDoneStream(): AsyncGenerator<StreamEvent> {
    yield { type: "done" };
  }

  beforeEach(() => {
    streamProviderChatMock.mockReset();
    streamProviderChatMock.mockImplementation(() => fakeDoneStream());
    useCostControlStore.setState({
      policy: { ...DEFAULT_COST_BUDGET_POLICY },
      rates: {},
      entries: [],
    });
  });

  it.each(["anthropic", "openai", "gemini", "openrouter"])(
    "forwards the resolved effort to the provider proxy for %s targets",
    async (providerId) => {
      const target: ResolvedTarget = { kind: "provider", providerId, model: "some-model" };

      await attemptStream(target, [], [], undefined, "max", "session-1");

      expect(streamProviderChatMock).toHaveBeenCalledTimes(1);
      expect(streamProviderChatMock.mock.calls[0][5]).toBe("max");
    },
  );

  it("still forwards effort for a custom provider — the Rust side omits it from that wire request", async () => {
    const target: ResolvedTarget = { kind: "provider", providerId: "my-custom-provider", model: "m" };

    await attemptStream(target, [], [], undefined, "high", "session-1");

    expect(streamProviderChatMock.mock.calls[0][5]).toBe("high");
  });

  it("forwards undefined (Default: no effort field at all) when the caller resolved none", async () => {
    const target: ResolvedTarget = { kind: "provider", providerId: "openai", model: "gpt-test" };

    await attemptStream(target, [], [], undefined, undefined, "session-1");

    expect(streamProviderChatMock.mock.calls[0][5]).toBeUndefined();
  });
});

// `isToolCallAllowed` now lives here (moved from `agentLoop.ts`, which
// re-exports it for backward compatibility — see that module's own doc
// comment) specifically so `subagent.ts`'s child tool-calling loop can reuse
// the exact same gate `agentLoop.ts`'s parent loop applies, rather than a
// parallel/duplicated check. `agentLoop.test.ts` still separately covers
// this via its own import (proving the re-export is the same function).
describe("isToolCallAllowed", () => {
  function toolDef(name: string): ToolDef {
    return { type: "function", function: { name, description: "", parameters: { type: "object", properties: {} } } };
  }

  it("returns true when the tool call's name matches one of the offered tools", () => {
    expect(isToolCallAllowed(call("write_file"), [toolDef("write_file")])).toBe(true);
  });

  it("returns false when the tool call's name was never offered", () => {
    expect(isToolCallAllowed(call("write_file"), [toolDef("read_file")])).toBe(false);
  });
});

// The Privacy Firewall choke point (audit fix: "mid-turn tool results bypass
// firewall"). `attemptStream` is the single function every cloud-bound
// request in the app flows through — Compare, Crew, subagents, side tasks,
// translation, the eval judge, and every one-shot workbench flow included —
// so gating INSIDE it means a surface cannot forget to gate. `agentLoop.ts`
// passes `preGated: true` for the wires it already gated itself.
describe("attemptStream / privacy firewall choke point", () => {
  const providerTarget: ResolvedTarget = { kind: "provider", providerId: "openai", model: "gpt-test" };

  function report(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
    return {
      destination: "cloud_model",
      workspaceId: "global",
      verdict: "allow",
      findings: [],
      redactedPreview: "",
      originalSha256: "0".repeat(64),
      localOnlyFallbackAvailable: false,
      contentLength: 0,
      ...overrides,
    };
  }

  async function* fakeStream(): AsyncGenerator<StreamEvent> {
    yield { type: "delta", content: "ok" };
    yield { type: "done" };
  }

  beforeEach(() => {
    invokeMock.mockReset();
    streamProviderChatMock.mockReset();
    streamProviderChatMock.mockImplementation(() => fakeStream());
    useCostControlStore.setState({ policy: { ...DEFAULT_COST_BUDGET_POLICY }, rates: {}, entries: [] });
    usePrivacyFirewallStore.setState({ pendingApproval: null, error: null });
  });

  it("sends the redacted wire copy when the verdict is redact, leaving the caller's original messages untouched", async () => {
    invokeMock.mockImplementation(async (command: unknown, args: unknown) => {
      if (command === "privacy_firewall_preview") {
        const { content } = args as { content: string };
        return report({
          verdict: content.includes("sk-secret") ? "redact" : "allow",
          redactedPreview: content.split("sk-secret").join("[REDACTED]"),
          findings: content.includes("sk-secret")
            ? [{ kind: "api_credential", byteStart: 0, byteEnd: 9, line: 1, column: 1, maskedPreview: "sk-…", action: "redact", exempted: false }]
            : [],
        });
      }
      throw new Error(`Unexpected invoke: ${String(command)}`);
    });
    const original = [{ role: "user" as const, content: "key is sk-secret please use it" }];

    const result = await attemptStream(providerTarget, original, [], undefined, undefined, "session-privacy");

    expect(result.streamError).toBeNull();
    const sentWire = streamProviderChatMock.mock.calls[0][2] as Array<{ content: string }>;
    expect(sentWire[0].content).toBe("key is [REDACTED] please use it");
    expect(original[0].content).toBe("key is sk-secret please use it");
  });

  it("fails CLOSED — nothing is sent — when the privacy preview itself errors", async () => {
    invokeMock.mockRejectedValue(new Error("scanner unavailable"));

    const result = await attemptStream(
      providerTarget,
      [{ role: "user", content: "contains something" }],
      [],
      undefined,
      undefined,
      "session-privacy",
    );

    expect(result.streamError).toMatch(/Privacy Firewall could not inspect/);
    expect(streamProviderChatMock).not.toHaveBeenCalled();
  });

  it("refuses to send when the user chooses switch-local on a require_approval verdict (one-shot surfaces cannot switch targets)", async () => {
    invokeMock.mockImplementation(async (command: unknown) => {
      if (command === "privacy_firewall_preview") return report({ verdict: "require_approval" });
      if (command === "privacy_firewall_prepare_send") {
        return { digest: "d".repeat(64), confirmationPhrase: "CONFIRM dddd", report: report({ verdict: "require_approval" }), expiresAtMs: Date.now() + 60_000 };
      }
      throw new Error(`Unexpected invoke: ${String(command)}`);
    });

    const pending = attemptStream(
      providerTarget,
      [{ role: "user", content: "protected" }],
      [],
      undefined,
      undefined,
      "session-privacy",
    );
    await vi.waitFor(() => {
      if (!usePrivacyFirewallStore.getState().pendingApproval) throw new Error("gate not pending yet");
    });
    await usePrivacyFirewallStore.getState().resolveDecision("switch_local");

    const result = await pending;
    expect(result.streamError).toMatch(/local model/i);
    expect(streamProviderChatMock).not.toHaveBeenCalled();
  });

  it("skips the redundant second scan when the caller marked the wire preGated", async () => {
    await attemptStream(
      providerTarget,
      [{ role: "user", content: "already gated upstream" }],
      [],
      undefined,
      undefined,
      "session-privacy",
      undefined,
      true,
      undefined,
      undefined,
      true,
      { preGated: true },
    );

    expect(invokeMock).not.toHaveBeenCalledWith("privacy_firewall_preview", expect.anything());
    expect(streamProviderChatMock).toHaveBeenCalled();
  });

  it("never consults the firewall for local/Ollama targets — nothing leaves the machine", async () => {
    const localTarget: ResolvedTarget = { kind: "ollama", baseUrl: "http://127.0.0.1:11434", model: "llama3.2" };
    // streamChat (local transport) is not mocked here; reaching it would throw
    // fetch errors, which is fine — the assertion below only cares that the
    // privacy preview was never invoked for a local target.
    await attemptStream(localTarget, [{ role: "user", content: "sk-secret stays local" }], [], undefined, undefined, "session-privacy").catch(() => undefined);

    expect(invokeMock).not.toHaveBeenCalledWith("privacy_firewall_preview", expect.anything());
  });
});

// User hooks wrap `executeToolCall`'s whole dispatch (see `userHooks.ts`):
// an explicit PreToolUse deny must block the call BEFORE any `invoke`
// dispatch and become the tool error; hook infrastructure failure must
// proceed. Store-seeded directly — the store's own load/save is
// `userHooks.test.ts`'s subject.
describe("executeToolCall / user hooks", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    useUserHooksStore.setState({ hooks: [], loaded: true });
  });

  it("blocks the tool call and returns the hook's reason as the tool error on an explicit deny", async () => {
    useUserHooksStore.setState({
      hooks: [{ id: "h1", event: "PreToolUse", command: "guard.sh", matcher: "write_file" }],
      loaded: true,
    });
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "hook_exec") return { exit_code: 2, stdout: "", stderr: "protected file", timed_out: false };
      throw new Error(`unexpected invoke: ${command}`);
    });

    const result = await executeToolCall(call("write_file", { path: "a.txt", content: "x" }), null, "turn-1", emptyMcpRegistry);

    const parsed = JSON.parse(result) as { error: string };
    expect(parsed.error).toContain("protected file");
    // The gate held: nothing but the hook itself was ever invoked.
    expect(invokeMock.mock.calls.every(([name]) => name === "hook_exec")).toBe(true);
  });

  it("proceeds to the normal dispatch when the hook times out", async () => {
    useUserHooksStore.setState({
      hooks: [{ id: "h1", event: "PreToolUse", command: "guard.sh" }],
      loaded: true,
    });
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "hook_exec") return { exit_code: null, stdout: "", stderr: "", timed_out: true };
      if (command === "tool_read_file") return "file contents";
      throw new Error(`unexpected invoke: ${command}`);
    });

    const result = await executeToolCall(call("read_file", { path: "a.txt" }), null, "turn-1", emptyMcpRegistry);

    expect(result).toContain("file contents");
  });

  it("fires PostToolUse hooks with the result after a successful dispatch", async () => {
    useUserHooksStore.setState({
      hooks: [{ id: "h2", event: "PostToolUse", command: "log.sh" }],
      loaded: true,
    });
    const hookPayloads: string[] = [];
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "hook_exec") {
        hookPayloads.push((args as { payload: string }).payload);
        return { exit_code: 0, stdout: "", stderr: "", timed_out: false };
      }
      if (command === "tool_read_file") return "file contents";
      throw new Error(`unexpected invoke: ${command}`);
    });

    await executeToolCall(call("read_file", { path: "a.txt" }), null, "turn-1", emptyMcpRegistry);

    await vi.waitFor(() => expect(hookPayloads.length).toBe(1));
    const payload = JSON.parse(hookPayloads[0]) as Record<string, unknown>;
    expect(payload.event).toBe("PostToolUse");
    expect(payload.tool_name).toBe("read_file");
    expect(typeof payload.result).toBe("string");
  });

  it("never consults hooks when none are configured", async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "tool_read_file") return "file contents";
      throw new Error(`unexpected invoke: ${command}`);
    });

    await executeToolCall(call("read_file", { path: "a.txt" }), null, "turn-1", emptyMcpRegistry);

    expect(invokeMock.mock.calls.some(([name]) => name === "hook_exec")).toBe(false);
  });
});

describe("executeToolCall / Plan Mode dispatch backstop", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    usePermissionStore.setState({ mode: "plan" });
  });

  afterEach(() => {
    usePermissionStore.setState({ mode: "manual" });
  });

  it.each(["write_file", "edit_file", "run_shell", "shell_kill", "remember", "web_fetch", "web_search"])(
    "refuses %s without dispatching to Rust",
    async (name) => {
      const result = await executeToolCall(call(name, { path: "a.ts" }), null, "turn-1", emptyMcpRegistry);
      expect(JSON.parse(result).error).toContain("Plan Mode");
      expect(invokeMock).not.toHaveBeenCalled();
    },
  );

  it("refuses an mcp__ tool without resolving or dispatching it", async () => {
    const result = await executeToolCall(call("mcp__srv__write_row"), null, "turn-1", emptyMcpRegistry);
    expect(JSON.parse(result).error).toContain("Plan Mode");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("still dispatches read-only tools in plan mode", async () => {
    invokeMock.mockResolvedValue("file contents");
    const result = await executeToolCall(call("read_file", { path: "a.ts" }), null, "turn-1", emptyMcpRegistry);
    expect(result).toBe("file contents");
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("dispatches the same mutating call normally once the mode is no longer plan — approval re-enables acting", async () => {
    usePermissionStore.setState({ mode: "acceptEdits" });
    invokeMock.mockResolvedValue("ok");
    const result = await executeToolCall(call("write_file", { path: "a.ts", content: "x" }), null, "turn-1", emptyMcpRegistry);
    expect(result).toBe("ok");
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("isBlockedInPlanMode: pins the exact blocked-name predicate", () => {
    for (const name of ["write_file", "edit_file", "run_shell", "shell_kill", "remember", "web_fetch", "web_search", "mcp__x__y"]) {
      expect(isBlockedInPlanMode(name), name).toBe(true);
    }
    for (const name of ["read_file", "list_dir", "glob", "grep", "shell_output", "task", "workflow", "skill", "present_plan", "spawn_task"]) {
      expect(isBlockedInPlanMode(name), name).toBe(false);
    }
  });
});

describe("executeToolCall / workspace_root_override reserved arg", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    usePermissionStore.setState({ mode: "manual" });
  });

  it("injects the frontend-supplied override for fs/shell tools", async () => {
    invokeMock.mockResolvedValue("ok");
    await executeToolCall(
      call("write_file", { path: "a.ts", content: "x" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      "/data/agent-worktrees/wt-1",
    );
    const [, args] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(args.workspace_root_override).toBe("/data/agent-worktrees/wt-1");
  });

  it("scrubs a model-supplied override — the model can never choose its own root", async () => {
    invokeMock.mockResolvedValue("ok");
    await executeToolCall(
      call("read_file", { path: "a.ts", workspace_root_override: "/etc" }),
      null,
      "turn-1",
      emptyMcpRegistry,
    );
    const [, args] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(args.workspace_root_override).toBeUndefined();
  });

  it("never attaches the override to tools with no workspace path (web_fetch)", async () => {
    invokeMock.mockResolvedValue("ok");
    await executeToolCall(
      call("web_fetch", { url: "https://example.com" }),
      null,
      "turn-1",
      emptyMcpRegistry,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      "/data/agent-worktrees/wt-1",
    );
    const [, args] = invokeMock.mock.calls[0] as [string, Record<string, unknown>];
    expect(args.workspace_root_override).toBeUndefined();
  });
});
