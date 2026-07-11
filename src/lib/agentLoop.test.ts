import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import {
  checkpointChainBlockReason,
  formatMemoryNotice,
  formatVerifyNotice,
  isMemoryNotice,
  isSuccessfulMutationResult,
  isToolCallAllowed,
  isVerifyNotice,
  parseMemoryNotice,
  parseVerifyNotice,
  runVerificationPhase,
  shouldFeedBackVerifyFailure,
  toolCallPathArg,
  toolsForSettings,
  type CheckpointChainLink,
  type MemoryNotice,
  type VerifyFailure,
  type VerifyNotice,
} from "./agentLoop";
import type { ChatMessage, ToolCall, ToolDef } from "./llamaClient";
import { useSettingsStore } from "../store/settingsStore";
import { usePermissionStore } from "../store/permissionStore";

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

describe("verify notices", () => {
  const notice: VerifyNotice = { label: "Lint", kind: "lint", ok: true, code: 0, output: "no problems found", durationMs: 1234 };

  it("formats a notice as a [Verify]-prefixed JSON payload and round-trips it back", () => {
    const formatted = formatVerifyNotice(notice);
    expect(formatted.startsWith("[Verify]")).toBe(true);

    const message: ChatMessage = { role: "system", content: formatted };
    expect(isVerifyNotice(message)).toBe(true);
    expect(parseVerifyNotice(message)).toEqual(notice);
  });

  it("round-trips a failing result", () => {
    const failed: VerifyNotice = { label: "Tests", kind: "test", ok: false, code: 1, output: "1 failing", durationMs: 500 };
    const message: ChatMessage = { role: "system", content: formatVerifyNotice(failed) };
    expect(parseVerifyNotice(message)).toEqual(failed);
  });

  it("is not misidentified as a verify notice for other message shapes", () => {
    expect(isVerifyNotice({ role: "system", content: "[Checkpoint]{}" })).toBe(false);
    expect(isVerifyNotice({ role: "user", content: "[Verify]{}" })).toBe(false);
    expect(parseVerifyNotice({ role: "assistant", content: "hello" })).toBeNull();
  });

  it("returns null for a malformed JSON payload instead of throwing", () => {
    const message: ChatMessage = { role: "system", content: "[Verify]not-json" };
    expect(parseVerifyNotice(message)).toBeNull();
  });

  it("returns null when the payload is missing required fields", () => {
    const message: ChatMessage = { role: "system", content: `[Verify]${JSON.stringify({ label: "only-label" })}` };
    expect(parseVerifyNotice(message)).toBeNull();
  });
});

describe("isSuccessfulMutationResult", () => {
  it("treats a plain-string success result (write_file/edit_file's actual shape) as successful", () => {
    expect(isSuccessfulMutationResult("Wrote 42 bytes to src/foo.ts")).toBe(true);
    expect(isSuccessfulMutationResult("Edited src/foo.ts")).toBe(true);
  });

  it("treats the {\"error\": ...} shape stringifyToolError produces as unsuccessful", () => {
    expect(isSuccessfulMutationResult(JSON.stringify({ error: "old_string not found in 'src/foo.ts'" }))).toBe(false);
  });

  it("treats arbitrary JSON without an error key as successful (only the error shape is excluded)", () => {
    expect(isSuccessfulMutationResult(JSON.stringify({ ok: true }))).toBe(true);
  });
});

describe("toolCallPathArg", () => {
  function call(args: unknown): ToolCall {
    return { id: "c1", type: "function", function: { name: "write_file", arguments: JSON.stringify(args) } };
  }

  it("extracts the path argument", () => {
    expect(toolCallPathArg(call({ path: "src/foo.ts", content: "x" }))).toBe("src/foo.ts");
  });

  it("returns null when arguments are malformed JSON", () => {
    const toolCall: ToolCall = { id: "c1", type: "function", function: { name: "write_file", arguments: "{not json" } };
    expect(toolCallPathArg(toolCall)).toBeNull();
  });

  it("returns null when there is no path argument", () => {
    expect(toolCallPathArg(call({ content: "x" }))).toBeNull();
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

  const toolsWithWeb = [toolDef("write_file"), toolDef("web_fetch"), toolDef("web_search"), toolDef("run_shell")];

  it("keeps web_fetch and web_search when webToolsEnabled is true (or omitted)", () => {
    expect(toolsForSettings(toolsWithWeb, true, true).map((t) => t.function.name)).toEqual([
      "write_file",
      "web_fetch",
      "web_search",
      "run_shell",
    ]);
    expect(toolsForSettings(toolsWithWeb, true).map((t) => t.function.name)).toEqual([
      "write_file",
      "web_fetch",
      "web_search",
      "run_shell",
    ]);
  });

  it("filters both web_fetch and web_search out when webToolsEnabled is false, leaving every other tool untouched", () => {
    expect(toolsForSettings(toolsWithWeb, true, false).map((t) => t.function.name)).toEqual(["write_file", "run_shell"]);
  });

  it("applies the memoryEnabled and webToolsEnabled filters independently", () => {
    const all = [toolDef("remember"), toolDef("web_fetch"), toolDef("web_search"), toolDef("write_file")];
    expect(toolsForSettings(all, false, false).map((t) => t.function.name)).toEqual(["write_file"]);
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

describe("runVerificationPhase", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    useSettingsStore.setState({ verifyEnabled: true });
    usePermissionStore.setState({ mode: "manual" });
  });

  it("no-ops without any IPC calls when verifyEnabled is off (report-only posture stays off by default)", async () => {
    useSettingsStore.setState({ verifyEnabled: false });
    const addMessage = vi.fn();

    const failure = await runVerificationPhase("turn-1", addMessage);

    expect(failure).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    expect(addMessage).not.toHaveBeenCalled();
  });

  it("never runs verification in plan mode, even with mutated files and verifyEnabled on", async () => {
    usePermissionStore.setState({ mode: "plan" });
    const addMessage = vi.fn();

    const failure = await runVerificationPhase("turn-1", addMessage);

    expect(failure).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    expect(addMessage).not.toHaveBeenCalled();
  });

  it("returns null and appends a passing notice when the only configured command succeeds", async () => {
    invokeMock.mockResolvedValueOnce({
      commands: [{ id: "cmd-1", label: "Lint", command: "pnpm lint", kind: "lint", enabled: true }],
    }); // verify_get_config
    invokeMock.mockResolvedValueOnce({
      commandId: "cmd-1",
      label: "Lint",
      kind: "lint",
      code: 0,
      stdout: "no problems found",
      stderr: "",
      durationMs: 10,
      timedOut: false,
    }); // verify_run

    const addMessage = vi.fn();
    const failure = await runVerificationPhase("turn-1", addMessage);

    expect(failure).toBeNull();
    expect(addMessage).toHaveBeenCalledTimes(1);
    const notice = parseVerifyNotice(addMessage.mock.calls[0][0] as ChatMessage);
    expect(notice?.ok).toBe(true);
  });

  it("returns the first failing command's details when a command fails", async () => {
    invokeMock.mockResolvedValueOnce({
      commands: [{ id: "cmd-1", label: "Tests", command: "pnpm test", kind: "test", enabled: true }],
    }); // verify_get_config
    invokeMock.mockResolvedValueOnce({
      commandId: "cmd-1",
      label: "Tests",
      kind: "test",
      code: 1,
      stdout: "",
      stderr: "1 failing",
      durationMs: 20,
      timedOut: false,
    }); // verify_run

    const addMessage = vi.fn();
    const failure = await runVerificationPhase("turn-1", addMessage);

    expect(failure).toEqual({ label: "Tests", code: 1, output: "1 failing" });
    const notice = parseVerifyNotice(addMessage.mock.calls[0][0] as ChatMessage);
    expect(notice?.ok).toBe(false);
  });
});

describe("shouldFeedBackVerifyFailure", () => {
  const failure: VerifyFailure = { label: "Tests", code: 1, output: "1 failing" };

  it("never feeds back a passing (null) result, regardless of the round budget", () => {
    expect(shouldFeedBackVerifyFailure(null, 0, 3)).toBe(false);
    expect(shouldFeedBackVerifyFailure(null, 0, 0)).toBe(false);
  });

  it("triggers exactly one feedback round when verifyMaxRounds is 1, then stops once the round is spent", () => {
    const maxRounds = 1;
    let verifyRound = 0;

    // First failure this turn: a round is still available.
    expect(shouldFeedBackVerifyFailure(failure, verifyRound, maxRounds)).toBe(true);
    verifyRound += 1; // mirrors runAgentTurnBody incrementing after appending the fix instruction

    // Same failure recurring after the fix round: budget is exhausted.
    expect(shouldFeedBackVerifyFailure(failure, verifyRound, maxRounds)).toBe(false);
  });

  it("never appends feedback when verifyMaxRounds is 0 (report-only)", () => {
    expect(shouldFeedBackVerifyFailure(failure, 0, 0)).toBe(false);
  });

  it("allows up to verifyMaxRounds rounds before exhausting the budget", () => {
    const maxRounds = 3;
    let verifyRound = 0;
    for (let i = 0; i < maxRounds; i++) {
      expect(shouldFeedBackVerifyFailure(failure, verifyRound, maxRounds)).toBe(true);
      verifyRound += 1;
    }
    expect(shouldFeedBackVerifyFailure(failure, verifyRound, maxRounds)).toBe(false);
  });
});
