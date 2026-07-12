import { beforeEach, describe, expect, it } from "vitest";

import { selectSubagentRun, useSubagentStore } from "./subagentStore";
import type { ChatMessage } from "../lib/llamaClient";

beforeEach(() => {
  useSubagentStore.setState({ runs: {} });
});

describe("subagentStore", () => {
  it("start registers a running run with zeroed activity", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "find X", profile: "explore" });

    const run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run).toEqual({
      sessionId: "s1",
      taskId: "t1",
      description: "find X",
      profile: "explore",
      status: "running",
      lastActivity: "",
      toolCallCount: 0,
      liveMessages: [],
    });
  });

  it("recordToolCall bumps toolCallCount and updates lastActivity, reflecting a running child's progress", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "find X", profile: "explore" });

    useSubagentStore.getState().recordToolCall("t1", 'grep("resolveTarget")');
    let run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run?.lastActivity).toBe('grep("resolveTarget")');
    expect(run?.toolCallCount).toBe(1);

    useSubagentStore.getState().recordToolCall("t1", "read_file(src/lib/tools.ts)");
    run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run?.lastActivity).toBe("read_file(src/lib/tools.ts)");
    expect(run?.toolCallCount).toBe(2);
  });

  it("recordToolCall is a no-op for an unregistered taskId", () => {
    useSubagentStore.getState().recordToolCall("missing", "grep(x)");
    expect(selectSubagentRun("missing")(useSubagentStore.getState())).toBeUndefined();
  });

  it("appendMessage grows liveMessages in order without mutating the previous array", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "find X", profile: "explore" });

    const m1: ChatMessage = { role: "assistant", content: "", tool_calls: [] };
    const m2: ChatMessage = { role: "tool", tool_call_id: "call-1", content: "result" };
    useSubagentStore.getState().appendMessage("t1", m1);
    const afterFirst = selectSubagentRun("t1")(useSubagentStore.getState())?.liveMessages;
    useSubagentStore.getState().appendMessage("t1", m2);
    const afterSecond = selectSubagentRun("t1")(useSubagentStore.getState())?.liveMessages;

    expect(afterFirst).toEqual([m1]);
    expect(afterSecond).toEqual([m1, m2]);
    // The array reference from the first snapshot must be untouched by the
    // second append (immutable update), not just "still equal by value".
    expect(afterFirst).not.toBe(afterSecond);
  });

  it("finish transitions status to a terminal value and leaves other fields untouched", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "find X", profile: "code" });
    useSubagentStore.getState().recordToolCall("t1", "write_file(a.ts)");

    useSubagentStore.getState().finish("t1", "done");

    const run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run?.status).toBe("done");
    expect(run?.toolCallCount).toBe(1);
    expect(run?.profile).toBe("code");
  });

  it("accumulateUsage sums onto a running total across multiple calls (slice 4)", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "find X", profile: "explore" });

    useSubagentStore.getState().accumulateUsage("t1", { promptTokens: 100, completionTokens: 20, totalTokens: 120 });
    let run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run?.usage).toEqual({ promptTokens: 100, completionTokens: 20, totalTokens: 120 });

    useSubagentStore.getState().accumulateUsage("t1", { promptTokens: 50, completionTokens: 10, totalTokens: 60 });
    run = selectSubagentRun("t1")(useSubagentStore.getState());
    expect(run?.usage).toEqual({ promptTokens: 150, completionTokens: 30, totalTokens: 180 });
  });

  it("accumulateUsage is a no-op for an unregistered taskId", () => {
    useSubagentStore.getState().accumulateUsage("missing", { promptTokens: 1, completionTokens: 1, totalTokens: 2 });
    expect(selectSubagentRun("missing")(useSubagentStore.getState())).toBeUndefined();
  });

  it("tracks multiple concurrent runs independently, keyed by taskId", () => {
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t1", description: "task one", profile: "explore" });
    useSubagentStore.getState().start({ sessionId: "s1", taskId: "t2", description: "task two", profile: "code" });

    useSubagentStore.getState().recordToolCall("t1", "grep(a)");
    useSubagentStore.getState().finish("t2", "error");

    const run1 = selectSubagentRun("t1")(useSubagentStore.getState());
    const run2 = selectSubagentRun("t2")(useSubagentStore.getState());
    expect(run1?.status).toBe("running");
    expect(run1?.toolCallCount).toBe(1);
    expect(run2?.status).toBe("error");
    expect(run2?.toolCallCount).toBe(0);
  });
});
