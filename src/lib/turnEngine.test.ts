import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { executeToolCall, PRESENT_PLAN_RESULT } from "./turnEngine";
import type { McpToolRegistry } from "./mcpTools";
import type { ToolCall } from "./llamaClient";

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
