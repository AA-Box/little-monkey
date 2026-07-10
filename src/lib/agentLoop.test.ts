import { describe, expect, it } from "vitest";

import {
  checkpointChainBlockReason,
  formatMemoryNotice,
  isMemoryNotice,
  isToolCallAllowed,
  parseMemoryNotice,
  toolsForSettings,
  type CheckpointChainLink,
  type MemoryNotice,
} from "./agentLoop";
import type { ChatMessage, ToolCall, ToolDef } from "./llamaClient";

function link(overrides: Partial<CheckpointChainLink> & { id: string }): CheckpointChainLink {
  return { shellRan: false, prevId: null, ...overrides };
}

describe("checkpointChainBlockReason", () => {
  it("returns null for an unbroken, shell-free chain", () => {
    // Newest-first, each correctly linking to the next-older survivor.
    const checkpoints = [
      link({ id: "c", prevId: "b" }),
      link({ id: "b", prevId: "a" }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
    expect(checkpointChainBlockReason(checkpoints, 1)).toBeNull();
    expect(checkpointChainBlockReason(checkpoints, 2)).toBeNull();
  });

  it("flags a pruned gap when a checkpoint's prevId doesn't match the next surviving entry", () => {
    // B was pruned: C's prevId still points at it, but the next surviving
    // entry is A.
    const checkpoints = [link({ id: "c", prevId: "b" }), link({ id: "a", prevId: null })];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("prunedGap");
    // The gap sits between index 0 and 1, so it must not affect a
    // "Restore to here" targeting only the newest checkpoint itself.
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });

  it("flags a shell run anywhere in the newest-to-target span", () => {
    const checkpoints = [
      link({ id: "c", prevId: "b" }),
      link({ id: "b", prevId: "a", shellRan: true }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("shellRan");
    expect(checkpointChainBlockReason(checkpoints, 2)).toBe("shellRan");
    // The shell run is at index 1, beyond a target of only the newest row.
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });

  it("prefers reporting a pruned gap over a shell run when both are present", () => {
    const checkpoints = [
      link({ id: "c", prevId: "b", shellRan: true }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("prunedGap");
  });

  it("does not flag a session's first checkpoint (null prevId) as a gap", () => {
    const checkpoints = [link({ id: "a", prevId: null })];
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });
});

describe("memory notices", () => {
  const notice: MemoryNotice = { id: "fact-1", text: "Uses pnpm, not npm." };

  it("formats a notice as a [Memory]-prefixed JSON payload and round-trips it back", () => {
    const formatted = formatMemoryNotice(notice);
    expect(formatted.startsWith("[Memory]")).toBe(true);

    const message: ChatMessage = { role: "system", content: formatted };
    expect(isMemoryNotice(message)).toBe(true);
    expect(parseMemoryNotice(message)).toEqual(notice);
  });

  it("round-trips the forgotten flag once the Forget button has been used", () => {
    const forgotten = formatMemoryNotice({ ...notice, forgotten: true });
    const message: ChatMessage = { role: "system", content: forgotten };
    expect(parseMemoryNotice(message)).toEqual({ ...notice, forgotten: true });
  });

  it("is not misidentified as a memory notice for other message shapes", () => {
    expect(isMemoryNotice({ role: "system", content: "[Checkpoint]{}" })).toBe(false);
    expect(isMemoryNotice({ role: "user", content: "[Memory]{}" })).toBe(false);
    expect(parseMemoryNotice({ role: "assistant", content: "hello" })).toBeNull();
  });

  it("returns null for a malformed JSON payload instead of throwing", () => {
    const message: ChatMessage = { role: "system", content: "[Memory]not-json" };
    expect(parseMemoryNotice(message)).toBeNull();
  });

  it("returns null when the payload is missing required fields", () => {
    const message: ChatMessage = { role: "system", content: `[Memory]${JSON.stringify({ id: "only-id" })}` };
    expect(parseMemoryNotice(message)).toBeNull();
  });
});

describe("toolsForSettings", () => {
  function toolDef(name: string): ToolDef {
    return { type: "function", function: { name, description: "", parameters: { type: "object", properties: {} } } };
  }

  const tools = [toolDef("write_file"), toolDef("remember"), toolDef("run_shell")];

  it("keeps every tool, including remember, when memoryEnabled is true", () => {
    expect(toolsForSettings(tools, true).map((t) => t.function.name)).toEqual(["write_file", "remember", "run_shell"]);
  });

  it("filters remember out when memoryEnabled is false, leaving every other tool untouched", () => {
    expect(toolsForSettings(tools, false).map((t) => t.function.name)).toEqual(["write_file", "run_shell"]);
  });

  it("is a no-op on a tool list that never had remember in it", () => {
    const noRemember = [toolDef("write_file"), toolDef("run_shell")];
    expect(toolsForSettings(noRemember, false)).toEqual(noRemember);
  });
});

describe("isToolCallAllowed", () => {
  function toolDef(name: string): ToolDef {
    return { type: "function", function: { name, description: "", parameters: { type: "object", properties: {} } } };
  }

  function call(name: string): ToolCall {
    return { id: "call-1", type: "function", function: { name, arguments: "{}" } };
  }

  const toolsForTurn = [toolDef("write_file"), toolDef("run_shell")];

  it("allows a call whose name was offered this turn", () => {
    expect(isToolCallAllowed(call("write_file"), toolsForTurn)).toBe(true);
  });

  it("rejects a call for a tool that was filtered out this turn (e.g. remember with memoryEnabled off)", () => {
    expect(isToolCallAllowed(call("remember"), toolsForTurn)).toBe(false);
  });

  it("allows remember once it's actually part of the offered tools", () => {
    const withRemember = [...toolsForTurn, toolDef("remember")];
    expect(isToolCallAllowed(call("remember"), withRemember)).toBe(true);
  });

  it("rejects a hallucinated tool name that was never offered at all", () => {
    expect(isToolCallAllowed(call("delete_everything"), toolsForTurn)).toBe(false);
  });
});
