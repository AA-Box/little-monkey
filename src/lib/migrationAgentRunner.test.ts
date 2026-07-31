import { beforeEach, describe, expect, it, vi } from "vitest";
import { errorMessage } from "./errors";

// `runMigrationSliceAgent` delegates to the shared headless
// model->tools->model loop, which reaches `turnEngine.ts`'s
// `attemptStream`/`executeToolCall`. Mock those primitives here so the public
// migration entry point still pins termination, iteration-cap, and
// cancellation behavior without needing a real streaming provider.
const mocks = vi.hoisted(() => ({
  resolveTarget: vi.fn(),
  snapshotForResolvedTarget: vi.fn(),
  effortForTarget: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  // Outside the Tauri shell, `durableRun.ts`'s `beginDurableRun` (transitively
  // reached through this module) resolves to `null` immediately — no need to
  // mock the Run Capsule ledger itself for these tests.
  isTauri: () => false,
}));

vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
  snapshotForResolvedTarget: (...args: unknown[]) => mocks.snapshotForResolvedTarget(...args),
}));

vi.mock("../store/modelStore", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../store/modelStore")>()),
  effortForTarget: (...args: unknown[]) => mocks.effortForTarget(...args),
}));

vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
  executeToolCall: (...args: unknown[]) => mocks.executeToolCall(...args),
  isToolCallAllowed: (toolCall: { function: { name: string } }, toolsForTurn: { function: { name: string } }[]) =>
    toolsForTurn.some((tool) => tool.function.name === toolCall.function.name),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: "Cancelled by the user" }),
  stringifyToolError: (err: unknown) => JSON.stringify({ error: errorMessage(err) }),
}));

import { MAX_MIGRATION_SLICE_ITERATIONS, runMigrationSliceAgent, type RunMigrationSliceParams } from "./migrationAgentRunner";
import type { ResolvedTarget } from "./turnEngine";
import type { ToolCall } from "./llamaClient";
import type { MigrationSlice } from "./migrationAgent";

const fakeTarget: ResolvedTarget = { kind: "local", baseUrl: "http://localhost:8090", modelLabel: "Local" };

function fixtureSlice(overrides: Partial<MigrationSlice> = {}): MigrationSlice {
  return {
    id: "slice-1",
    order: 1,
    title: "Bump the dependency",
    description: "Update package.json to react@19",
    riskLevel: "medium",
    riskNotes: ["Peer dependency conflicts possible"],
    rollbackNotes: "Revert package.json",
    filesLikely: ["package.json"],
    ...overrides,
  };
}

function baseParams(overrides: Partial<RunMigrationSliceParams> = {}): RunMigrationSliceParams {
  return {
    runId: "run-1",
    goal: "Upgrade React to v19",
    slice: fixtureSlice(),
    branch: "codex/migration/run-1",
    workspaceLabel: "wt-1",
    signal: new AbortController().signal,
    ...overrides,
  };
}

function toolCall(name: string, id = "call-1"): ToolCall {
  return { id, type: "function", function: { name, arguments: "{}" } };
}

beforeEach(() => {
  mocks.resolveTarget.mockReset();
  mocks.snapshotForResolvedTarget.mockReset();
  mocks.effortForTarget.mockReset();
  mocks.attemptStream.mockReset();
  mocks.executeToolCall.mockReset();
  mocks.resolveTarget.mockResolvedValue(fakeTarget);
  mocks.snapshotForResolvedTarget.mockReturnValue(null);
  mocks.effortForTarget.mockReturnValue(undefined);
});

describe("runMigrationSliceAgent / termination", () => {
  it("returns the agent's final summary once it stops requesting tool calls", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: "Bumped react to 19.0.0 and ran the test suite: all green.",
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    const result = await runMigrationSliceAgent(baseParams());

    expect(result.outcome).toBe("completed");
    expect(result.summary).toContain("all green");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
    expect(mocks.executeToolCall).not.toHaveBeenCalled();
  });

  it("includes the slice title/goal/branch/workspace label in the system prompt", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "done", toolCalls: [], streamError: null, contentStarted: true });
    await runMigrationSliceAgent(baseParams());
    const wireHistory = mocks.attemptStream.mock.calls[0][1] as { role: string; content: string }[];
    const system = wireHistory.find((m) => m.role === "system")?.content ?? "";
    expect(system).toContain("Upgrade React to v19");
    expect(system).toContain("Bump the dependency");
    expect(system).toContain("codex/migration/run-1");
    expect(system).toContain("wt-1/");
  });

  it("executes tool calls the model requests and feeds results back", async () => {
    mocks.attemptStream
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [toolCall("run_shell")],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "Done.", toolCalls: [], streamError: null, contentStarted: true });
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ ok: true }));

    const result = await runMigrationSliceAgent(baseParams());

    expect(result.outcome).toBe("completed");
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    expect(mocks.executeToolCall.mock.calls[0][8]).toBe("migration-agent");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
  });

  it("stops with an error outcome on a stream error", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "", toolCalls: [], streamError: "Model unavailable", contentStarted: false });
    const result = await runMigrationSliceAgent(baseParams());
    expect(result.outcome).toBe("error");
    expect(result.summary).toBe("Model unavailable");
  });

  it("stops with a cancelled outcome when the signal is already aborted", async () => {
    const controller = new AbortController();
    controller.abort();
    const result = await runMigrationSliceAgent(baseParams({ signal: controller.signal }));
    expect(result.outcome).toBe("cancelled");
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("reports an error after exceeding the iteration cap", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: "",
      toolCalls: [toolCall("run_shell")],
      streamError: null,
      contentStarted: true,
    });
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ ok: true }));

    const result = await runMigrationSliceAgent(baseParams());

    expect(result.outcome).toBe("error");
    expect(result.summary).toContain(String(MAX_MIGRATION_SLICE_ITERATIONS));
    expect(mocks.attemptStream).toHaveBeenCalledTimes(MAX_MIGRATION_SLICE_ITERATIONS);
  });

  it("reports a tool call not offered to this run as a tool error rather than executing it", async () => {
    mocks.attemptStream
      .mockResolvedValueOnce({ content: "", toolCalls: [toolCall("task")], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: "done", toolCalls: [], streamError: null, contentStarted: true });

    const result = await runMigrationSliceAgent(baseParams());

    expect(result.outcome).toBe("completed");
    expect(mocks.executeToolCall).not.toHaveBeenCalled();
  });
});
