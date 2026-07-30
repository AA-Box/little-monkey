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
