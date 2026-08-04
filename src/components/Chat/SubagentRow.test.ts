import { describe, expect, it } from "vitest";

// This codebase's vitest config (`vitest.config.ts`) runs under the `node`
// environment with `include: ["src/**/*.test.ts"]` — there is no DOM/React-
// rendering harness (no `jsdom`/`happy-dom`, no `@testing-library/react`),
// and no OTHER component in this app has a test either. Standing up that
// infrastructure is out of scope for this slice, so — deviating from the
// design doc's literal "SubagentRow renders correctly for each status" ask —
// this file instead exhaustively covers every pure function that DETERMINES
// what `SubagentRow.tsx` renders for each status/profile/transcript shape,
// exported from that module for exactly this purpose. See that module's own
// top comments on `parseTaskArgs`/`resolveSubagentStatus` for the same note.
import {
  extractChildToolCalls,
  groupChildToolCalls,
  parseTaskArgs,
  resolveSubagentStatus,
  statusLabelKey,
} from "./SubagentRow";
import { CANCELLED_TOOL_RESULT } from "../../lib/turnEngine";
import type { ChatMessage } from "../../lib/llamaClient";
import { protectToolResult } from "../../lib/untrustedContent";

describe("parseTaskArgs", () => {
  it("parses a well-formed task call's description and profile", () => {
    expect(parseTaskArgs(JSON.stringify({ description: "find X", prompt: "...", profile: "explore" }))).toEqual({
      description: "find X",
      profile: "explore",
    });
  });

  it("recognizes the 'code' profile", () => {
    expect(parseTaskArgs(JSON.stringify({ description: "fix bug", profile: "code" }))).toEqual({
      description: "fix bug",
      profile: "code",
    });
  });

  it("defaults profile to 'explore' for any value other than the literal 'code'", () => {
    expect(parseTaskArgs(JSON.stringify({ description: "x", profile: "something-else" })).profile).toBe("explore");
    expect(parseTaskArgs(JSON.stringify({ description: "x" })).profile).toBe("explore");
  });

  it("falls back to a default description for malformed JSON, empty string, or a missing/blank field", () => {
    expect(parseTaskArgs("not json")).toEqual({ description: "Subagent task", profile: "explore" });
    expect(parseTaskArgs("")).toEqual({ description: "Subagent task", profile: "explore" });
    expect(parseTaskArgs(JSON.stringify({ description: "   " }))).toEqual({ description: "Subagent task", profile: "explore" });
    expect(parseTaskArgs(JSON.stringify({ profile: "code" }))).toEqual({ description: "Subagent task", profile: "code" });
  });
});

describe("statusLabelKey", () => {
  it("maps every SubagentStatus to its own i18n key", () => {
    expect(statusLabelKey("running")).toBe("SubagentRow.statusRunning");
    expect(statusLabelKey("done")).toBe("SubagentRow.statusDone");
    expect(statusLabelKey("error")).toBe("SubagentRow.statusFailed");
    expect(statusLabelKey("cancelled")).toBe("SubagentRow.statusCancelled");
  });
});

describe("resolveSubagentStatus", () => {
  it("prefers the live subagentStore status when one exists, regardless of the tool result", () => {
    expect(resolveSubagentStatus("running", "some report")).toBe("running");
    expect(resolveSubagentStatus("done", undefined)).toBe("done");
    expect(resolveSubagentStatus("error", "some report")).toBe("error");
    expect(resolveSubagentStatus("cancelled", "some report")).toBe("cancelled");
  });

  it("falls back to 'running' when there's no live entry and no result yet (call still in flight)", () => {
    expect(resolveSubagentStatus(undefined, undefined)).toBe("running");
  });

  it("falls back to 'cancelled' for an exact CANCELLED_TOOL_RESULT match, without a live entry", () => {
    expect(resolveSubagentStatus(undefined, CANCELLED_TOOL_RESULT)).toBe("cancelled");
  });

  it("keeps a persisted protected cancellation classified as cancelled", () => {
    expect(
      resolveSubagentStatus(
        undefined,
        protectToolResult("task", CANCELLED_TOOL_RESULT),
      ),
    ).toBe("cancelled");
  });

  it("falls back to 'error' for any other error-shaped JSON result, without a live entry", () => {
    expect(resolveSubagentStatus(undefined, JSON.stringify({ error: "network broke" }))).toBe("error");
  });

  it("falls back to 'done' for a plain report string, without a live entry", () => {
    expect(resolveSubagentStatus(undefined, "Found 3 callers of X.")).toBe("done");
  });
});

describe("extractChildToolCalls", () => {
  it("returns an empty list for a transcript with no tool calls", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "find every caller of X" },
      { role: "assistant", content: "Found 3 callers of X." },
    ];
    expect(extractChildToolCalls(messages)).toEqual([]);
  });

  it("pairs each assistant tool_call with its matching tool result, in order", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "find every caller of X" },
      {
        role: "assistant",
        content: "",
        tool_calls: [
          { id: "call-1", type: "function", function: { name: "grep", arguments: '{"pattern":"X"}' } },
          { id: "call-2", type: "function", function: { name: "read_file", arguments: '{"path":"a.ts"}' } },
        ],
      },
      { role: "tool", tool_call_id: "call-1", content: "3 matches" },
      { role: "tool", tool_call_id: "call-2", content: "file contents" },
      { role: "assistant", content: "Found 3 callers of X." },
    ];

    expect(extractChildToolCalls(messages)).toEqual([
      { key: "call-1", name: "grep", args: '{"pattern":"X"}', result: "3 matches" },
      { key: "call-2", name: "read_file", args: '{"path":"a.ts"}', result: "file contents" },
    ]);
  });

  it("flattens every group's calls, in order", () => {
    const messages: ChatMessage[] = [
      { role: "assistant", content: "looking", tool_calls: [{ id: "call-1", type: "function", function: { name: "grep", arguments: "{}" } }] },
      { role: "tool", tool_call_id: "call-1", content: "hit" },
      { role: "assistant", content: "reading", tool_calls: [{ id: "call-2", type: "function", function: { name: "read_file", arguments: "{}" } }] },
    ];
    expect(extractChildToolCalls(messages).map((row) => row.key)).toEqual(["call-1", "call-2"]);
  });

  it("leaves result undefined for a tool call still in flight (no matching tool message yet)", () => {
    const messages: ChatMessage[] = [
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-1", type: "function", function: { name: "grep", arguments: "{}" } }],
      },
    ];
    const rows = extractChildToolCalls(messages);
    expect(rows).toHaveLength(1);
    expect(rows[0].result).toBeUndefined();
  });
});

describe("groupChildToolCalls", () => {
  it("makes one titled group per assistant round, taking the round's own narration", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "audit the runtime" },
      {
        role: "assistant",
        content: "Found binary download machinery\nand a checksum helper",
        tool_calls: [
          { id: "call-1", type: "function", function: { name: "grep", arguments: '{"pattern":"sha256"}' } },
          { id: "call-2", type: "function", function: { name: "read_file", arguments: '{"path":"a.rs"}' } },
        ],
      },
      { role: "tool", tool_call_id: "call-1", content: "3 matches" },
      { role: "tool", tool_call_id: "call-2", content: "file contents" },
      {
        role: "assistant",
        content: "Checked licenses",
        tool_calls: [{ id: "call-3", type: "function", function: { name: "read_file", arguments: '{"path":"LICENSE"}' } }],
      },
    ];

    const groups = groupChildToolCalls(messages);
    expect(groups).toHaveLength(2);
    // Only the first line of a multi-line narration becomes the header.
    expect(groups[0].title).toBe("Found binary download machinery");
    expect(groups[0].key).toBe("call-1");
    expect(groups[0].calls.map((call) => call.key)).toEqual(["call-1", "call-2"]);
    expect(groups[0].calls[0].result).toBe("3 matches");
    expect(groups[1].title).toBe("Checked licenses");
    expect(groups[1].calls).toHaveLength(1);
  });

  it("carries a text-only round's narration forward to the next round that calls tools, once", () => {
    const messages: ChatMessage[] = [
      { role: "assistant", content: "Next I'll inspect the manifest" },
      { role: "assistant", content: "", tool_calls: [{ id: "call-1", type: "function", function: { name: "read_file", arguments: "{}" } }] },
      { role: "assistant", content: "", tool_calls: [{ id: "call-2", type: "function", function: { name: "grep", arguments: "{}" } }] },
    ];

    const groups = groupChildToolCalls(messages);
    expect(groups.map((group) => group.title)).toEqual(["Next I'll inspect the manifest", null]);
  });

  it("leaves a round untitled when the child said nothing, and caps a long narration", () => {
    const long = "x".repeat(150);
    const messages: ChatMessage[] = [
      { role: "assistant", content: "   ", tool_calls: [{ id: "call-1", type: "function", function: { name: "grep", arguments: "{}" } }] },
      { role: "assistant", content: long, tool_calls: [{ id: "call-2", type: "function", function: { name: "grep", arguments: "{}" } }] },
    ];

    const groups = groupChildToolCalls(messages);
    expect(groups[0].title).toBeNull();
    expect(groups[1].title).toBe(`${"x".repeat(99)}…`);
  });

  it("ignores rounds with no tool calls at all", () => {
    expect(groupChildToolCalls([{ role: "assistant", content: "Found 3 callers of X." }])).toEqual([]);
  });
});
