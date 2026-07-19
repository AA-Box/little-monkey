import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  resolveTarget: vi.fn(),
  attemptStream: vi.fn(),
  discoverSkills: vi.fn(),
  loadWorkflow: vi.fn(),
  validateWorkflow: vi.fn(),
  runWorkflow: vi.fn(),
  cancelWorkflow: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke, isTauri: () => false }));
vi.mock("./agentLoop", () => ({
  resolveTarget: mocks.resolveTarget,
  allowedToolsRestriction: (_commands: Set<string>, skills: Array<{ allowedTools?: string[] }>) => {
    const allowed = skills[0]?.allowedTools;
    return allowed && allowed.length > 0 ? new Set(allowed) : null;
  },
  applyAllowedToolsRestriction: (tools: Array<{ function: { name: string } }>, restriction: Set<string> | null) =>
    restriction === null ? tools : tools.filter((tool) => restriction.has(tool.function.name)),
}));
vi.mock("./turnEngine", () => ({ attemptStream: mocks.attemptStream }));
vi.mock("./nativeSkillsClient", () => ({ nativeSkillsClient: { discover: mocks.discoverSkills } }));
vi.mock("./ecosystemClient", () => ({
  ecosystemClient: {
    loadWorkflow: mocks.loadWorkflow,
    validateWorkflow: mocks.validateWorkflow,
    runWorkflow: mocks.runWorkflow,
    cancelWorkflow: mocks.cancelWorkflow,
  },
}));
vi.mock("../store/modelStore", () => ({
  effortForTarget: () => "medium",
  getActiveChatTarget: () => null,
}));

import { createEvalCase, createLocalEvalRuntime } from "./evalHarness";

describe("createLocalEvalRuntime", () => {
  beforeEach(() => {
    for (const mock of Object.values(mocks)) mock.mockReset();
    mocks.resolveTarget.mockResolvedValue({ kind: "local", baseUrl: "http://127.0.0.1:8080", modelLabel: "Local fixture" });
    mocks.attemptStream.mockResolvedValue({
      content: "done",
      toolCalls: [{ id: "call-1", type: "function", function: { name: "lookup", arguments: "{}" } }],
      streamError: null,
      contentStarted: true,
      usage: { promptTokens: 5, completionTokens: 2, totalTokens: 7 },
    });
  });

  it("executes an agent case through the active model and captures dry-run tool calls", async () => {
    const testCase = createEvalCase("agent");
    testCase.input = "Find the record";
    testCase.context = "record id: 7";
    testCase.allowedTools = ["lookup"];

    const result = await createLocalEvalRuntime().execute({ kind: "agent" }, testCase, "run-agent", new AbortController().signal);

    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
    expect(mocks.attemptStream.mock.calls[0][2]).toMatchObject([{ function: { name: "lookup" } }]);
    expect(mocks.attemptStream.mock.calls[0][7]).toBe(false);
    expect(result).toMatchObject({ output: "done", toolCalls: ["lookup"], targetLabel: "Local fixture", metadata: { mode: "dry-run-tool-capture" } });
  });

  it("loads and applies an installed native skill before executing it", async () => {
    mocks.discoverSkills.mockResolvedValue([{
      name: "Summarize",
      description: "Summarize input",
      command: "summarize",
      version: "1.0.0",
      instructions: "Always return one sentence.",
      sha256: "a".repeat(64),
      file_count: 1,
      total_bytes: 100,
      enabled: true,
      eligibility: { eligible: true, current_os: "macos", unsupported_os: false, missing_bins: [], missing_env: [] },
      supported_os: [],
      requirements: { bins: [], env: [] },
      source: { kind: "global", path: "/skills/summarize" },
      permissions: [],
      git_repository: null,
      allowed_tools: ["read_only"],
      resource_files: [],
    }]);
    const testCase = createEvalCase("skill");
    testCase.input = "A long fixture";
    testCase.allowedTools = ["read_only", "danger"];

    await createLocalEvalRuntime().execute({ kind: "skill", command: "summarize" }, testCase, "run-skill", new AbortController().signal);

    const messages = mocks.attemptStream.mock.calls[0][1] as Array<{ role: string; content: string }>;
    expect(messages[0].content).toContain("Always return one sentence.");
    expect(messages[1].content).toBe("A long fixture");
    expect(mocks.attemptStream.mock.calls[0][2]).toMatchObject([{ function: { name: "read_only" } }]);
  });

  it("previews connector replay without IPC and performs live connector calls with exact arguments", async () => {
    const testCase = createEvalCase("connector");
    testCase.input = '{"query":"widgets"}';
    testCase.dryRun = true;
    const runtime = createLocalEvalRuntime();

    const preview = await runtime.execute({ kind: "connector", serverId: "api", toolName: "search" }, testCase, "run-preview", new AbortController().signal);
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(JSON.parse(preview.output)).toMatchObject({ dryRun: true, serverId: "api", toolName: "search", arguments: { query: "widgets" } });

    testCase.dryRun = false;
    mocks.invoke.mockResolvedValue({ content: [{ type: "text", text: "two widgets" }], isError: false });
    const live = await runtime.execute({ kind: "connector", serverId: "api", toolName: "search" }, testCase, "run-live", new AbortController().signal);
    expect(mocks.invoke).toHaveBeenCalledWith("mcp_call_tool", {
      server_id: "api",
      tool_name: "search",
      arguments: { query: "widgets" },
      turn_id: "run-live",
      tool_call_id: `run-live-${testCase.id}`,
    });
    expect(live).toMatchObject({ output: "two widgets", executionSucceeded: true, toolCalls: ["api/search"] });
  });

  it("runs a saved workflow and maps outputs, executed nodes, usage, and cost into evidence", async () => {
    const testCase = createEvalCase("workflow");
    testCase.input = '{"count":2,"enabled":true}';
    mocks.runWorkflow.mockResolvedValue({
      run_id: `run-workflow-${testCase.id}`,
      workflow_id: "publish",
      definition_sha256: "sha",
      status: "succeeded",
      nodes: { transform: { node_id: "transform", attempts: 1 } },
      outputs: { result: { kind: "string", value: "ok" } },
      usage: { input_tokens: 4, output_tokens: 3, cost_microunits: 9 },
    });

    const result = await createLocalEvalRuntime().execute({ kind: "workflow", workflowId: "publish" }, testCase, "run-workflow", new AbortController().signal);

    expect(mocks.runWorkflow).toHaveBeenCalledWith("publish", expect.objectContaining({
      run_id: `run-workflow-${testCase.id}`,
      inputs: { count: { kind: "integer", value: 2 }, enabled: { kind: "boolean", value: true } },
      trigger: { kind: "manual" },
    }));
    expect(result).toMatchObject({ executionSucceeded: true, toolCalls: ["transform"], usage: { totalTokens: 7 }, costMicros: 9 });
  });

  it("derives the production judge boolean from score threshold", async () => {
    const testCase = createEvalCase("judge");
    testCase.input = "candidate";
    testCase.judgeRubric = "Must be correct.";
    testCase.judgeThreshold = 0.8;
    mocks.attemptStream.mockResolvedValue({
      content: '{"passed":true,"score":0.2,"evidence":"Incorrect fact."}',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
      usage: undefined,
    });

    const judged = await createLocalEvalRuntime().judge(
      testCase,
      { output: "wrong", toolCalls: [], usage: null, costMicros: null, executionSucceeded: true, targetLabel: "fixture", metadata: {} },
      "run-judge",
      new AbortController().signal,
    );

    expect(judged).toMatchObject({ passed: false, score: 0.2, evidence: "Incorrect fact." });
  });
});
