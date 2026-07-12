import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import {
  attachedStackPromptInfo,
  checkpointChainBlockReason,
  formatMemoryNotice,
  formatPlanNotice,
  formatSourcesNotice,
  formatVerifyNotice,
  isMemoryNotice,
  isPlanNotice,
  isSourcesNotice,
  isSuccessfulMutationResult,
  isToolCallAllowed,
  isVerifyFixNotice,
  isVerifyNotice,
  maybeAutoPreviewNewestArtifact,
  parseMemoryNotice,
  parsePlanNotice,
  parseSourcesNotice,
  parseVerifyNotice,
  PLAN_NOTE_PREFIX,
  runVerificationPhase,
  shouldFeedBackVerifyFailure,
  toolCallPathArg,
  toolCallPlanArgs,
  toolsForMode,
  toolsForSettings,
  VERIFY_FIX_NOTE_PREFIX,
  type CheckpointChainLink,
  type MemoryNotice,
  type PlanNotice,
  type SourcesNotice,
  type VerifyFailure,
  type VerifyNotice,
} from "./agentLoop";
import { estimateHistoryTokens } from "./contextTrimmer";
import type { ChatMessage, ToolCall, ToolDef } from "./llamaClient";
import { useSettingsStore } from "../store/settingsStore";
import { usePermissionStore } from "../store/permissionStore";
import { selectRunningVerifyLabel, selectTurnRunning, useSessionStore, type ChatSession } from "../store/sessionStore";
import { useArtifactStore } from "../store/artifactStore";

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

describe("sources notices", () => {
  const notice: SourcesNotice = {
    results: [
      { path: "docs/guide.md", stack: "Docs", score: 0.87, snippet: "Install via pnpm install." },
      { path: "docs/faq.md", stack: "Docs", score: 0.61, snippet: "See the FAQ for troubleshooting." },
    ],
  };

  it("formats a notice as a [Sources]-prefixed JSON payload and round-trips it back", () => {
    const formatted = formatSourcesNotice(notice);
    expect(formatted.startsWith("[Sources]")).toBe(true);

    const message: ChatMessage = { role: "system", content: formatted };
    expect(isSourcesNotice(message)).toBe(true);
    expect(parseSourcesNotice(message)).toEqual(notice);
  });

  it("round-trips an empty results list", () => {
    const empty: SourcesNotice = { results: [] };
    const message: ChatMessage = { role: "system", content: formatSourcesNotice(empty) };
    expect(parseSourcesNotice(message)).toEqual(empty);
  });

  it("is not misidentified as a sources notice for other message shapes", () => {
    expect(isSourcesNotice({ role: "system", content: "[Checkpoint]{}" })).toBe(false);
    expect(isSourcesNotice({ role: "user", content: "[Sources]{}" })).toBe(false);
    expect(parseSourcesNotice({ role: "assistant", content: "hello" })).toBeNull();
  });

  it("returns null for a malformed JSON payload instead of throwing", () => {
    const message: ChatMessage = { role: "system", content: "[Sources]not-json" };
    expect(parseSourcesNotice(message)).toBeNull();
  });

  it("returns null when a result entry is missing a required field", () => {
    const message: ChatMessage = {
      role: "system",
      content: `[Sources]${JSON.stringify({ results: [{ path: "a.md", stack: "Docs", score: 0.5 }] })}`,
    };
    expect(parseSourcesNotice(message)).toBeNull();
  });

  it("counts toward contextTrimmer's token estimate like any other message (RAG design doc's context-bloat risk)", () => {
    // A realistic doc-chat notice: 6 chunks at ~1600 chars each, per the
    // design doc's own budget note (~2.4k tokens at 4 chars/token).
    const bigNotice: SourcesNotice = {
      results: Array.from({ length: 6 }, (_, i) => ({
        path: `docs/file-${i}.md`,
        stack: "Docs",
        score: 0.9 - i * 0.05,
        snippet: "x".repeat(1600),
      })),
    };
    const message: ChatMessage = { role: "system", content: formatSourcesNotice(bigNotice) };
    // No special-casing needed: estimateHistoryTokens sums every message's
    // content length generically, so a large [Sources] notice already
    // contributes to the total exactly like a long tool result would.
    expect(estimateHistoryTokens([message])).toBeGreaterThan(2000);
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

describe("toolsForMode", () => {
  function toolDef(name: string): ToolDef {
    return { type: "function", function: { name, description: "", parameters: { type: "object", properties: {} } } };
  }

  const base = [toolDef("read_file"), toolDef("write_file")];

  it("appends present_plan only in plan mode", () => {
    expect(toolsForMode(base, "plan").map((t) => t.function.name)).toEqual(["read_file", "write_file", "present_plan"]);
  });

  it("returns the tool list unchanged (same reference) in every other mode", () => {
    for (const mode of ["manual", "acceptEdits", "smart", "auto", "bypass"] as const) {
      expect(toolsForMode(base, mode)).toBe(base);
    }
  });
});

describe("attachedStackPromptInfo", () => {
  it("reports chunk count for an indexed stack", () => {
    const info = attachedStackPromptInfo([{ name: "Docs", indexed_at: 12345, chunk_count: 42 }]);
    expect(info).toEqual([{ name: "Docs", description: "42 chunks indexed" }]);
  });

  it("uses singular phrasing for exactly one chunk", () => {
    const info = attachedStackPromptInfo([{ name: "Docs", indexed_at: 1, chunk_count: 1 }]);
    expect(info[0].description).toBe("1 chunk indexed");
  });

  it('reports "not indexed yet" for a stack that has never been indexed', () => {
    const info = attachedStackPromptInfo([{ name: "New Stack", indexed_at: null, chunk_count: 0 }]);
    expect(info).toEqual([{ name: "New Stack", description: "not indexed yet" }]);
  });

  it("maps multiple stacks in order", () => {
    const info = attachedStackPromptInfo([
      { name: "Docs", indexed_at: 1, chunk_count: 10 },
      { name: "Notes", indexed_at: null, chunk_count: 0 },
    ]);
    expect(info.map((s) => s.name)).toEqual(["Docs", "Notes"]);
  });

  it("returns an empty array for no attached stacks", () => {
    expect(attachedStackPromptInfo([])).toEqual([]);
  });
});

describe("PlanNotice: formatPlanNotice/parsePlanNotice round trip", () => {
  const notice: PlanNotice = {
    id: "p1",
    title: "Refactor auth",
    plan: "1. Extract the token check\n2. Add a test",
    openQuestions: ["Should old sessions be invalidated?"],
    status: "proposed",
  };

  it("round-trips a full notice through format -> parse", () => {
    const message: ChatMessage = { role: "system", content: formatPlanNotice(notice) };
    expect(isPlanNotice(message)).toBe(true);
    expect(parsePlanNotice(message)).toEqual(notice);
  });

  it("round-trips a notice with no open questions", () => {
    const { openQuestions: _openQuestions, ...withoutQuestions } = notice;
    const message: ChatMessage = { role: "system", content: formatPlanNotice(withoutQuestions as PlanNotice) };
    expect(parsePlanNotice(message)).toEqual(withoutQuestions);
  });

  it("round-trips the approved/dismissed status rewrites the Approve/Keep-planning buttons produce", () => {
    const approved: PlanNotice = { ...notice, status: "approved" };
    const dismissed: PlanNotice = { ...notice, status: "dismissed" };
    expect(parsePlanNotice({ role: "system", content: formatPlanNotice(approved) })).toEqual(approved);
    expect(parsePlanNotice({ role: "system", content: formatPlanNotice(dismissed) })).toEqual(dismissed);
  });

  it("does not misidentify an unrelated system message as a plan notice", () => {
    expect(isPlanNotice({ role: "system", content: "just a regular system message" })).toBe(false);
    expect(parsePlanNotice({ role: "system", content: "just a regular system message" })).toBeNull();
  });

  it("returns null for malformed JSON after the prefix", () => {
    expect(parsePlanNotice({ role: "system", content: `${PLAN_NOTE_PREFIX}{not json` })).toBeNull();
  });

  it("returns null for a well-formed but incomplete payload (missing plan)", () => {
    const badPayload = JSON.stringify({ id: "p1", title: "T", status: "proposed" });
    expect(parsePlanNotice({ role: "system", content: `${PLAN_NOTE_PREFIX}${badPayload}` })).toBeNull();
  });

  it("returns null when openQuestions is present but not an array (e.g. a corrupted/hand-edited persisted session)", () => {
    const badPayload = JSON.stringify({ id: "p1", title: "T", plan: "P", status: "proposed", openQuestions: "pending" });
    expect(parsePlanNotice({ role: "system", content: `${PLAN_NOTE_PREFIX}${badPayload}` })).toBeNull();
  });

  it("returns null when openQuestions is an array containing a non-string entry", () => {
    const badPayload = JSON.stringify({ id: "p1", title: "T", plan: "P", status: "proposed", openQuestions: ["ok", 5] });
    expect(parsePlanNotice({ role: "system", content: `${PLAN_NOTE_PREFIX}${badPayload}` })).toBeNull();
  });
});

describe("toolCallPlanArgs", () => {
  function call(args: unknown): ToolCall {
    return { id: "c1", type: "function", function: { name: "present_plan", arguments: JSON.stringify(args) } };
  }

  it("extracts title, plan, and open_questions", () => {
    expect(toolCallPlanArgs(call({ title: "T", plan: "P", open_questions: ["Q1", "Q2"] }))).toEqual({
      title: "T",
      plan: "P",
      openQuestions: ["Q1", "Q2"],
    });
  });

  it("omits openQuestions when open_questions is absent or empty", () => {
    expect(toolCallPlanArgs(call({ title: "T", plan: "P" }))).toEqual({ title: "T", plan: "P" });
    expect(toolCallPlanArgs(call({ title: "T", plan: "P", open_questions: [] }))).toEqual({ title: "T", plan: "P" });
  });

  it("filters non-string entries out of open_questions", () => {
    expect(toolCallPlanArgs(call({ title: "T", plan: "P", open_questions: ["Q1", 2, null, "Q2"] }))).toEqual({
      title: "T",
      plan: "P",
      openQuestions: ["Q1", "Q2"],
    });
  });

  it("returns null when title or plan is missing or not a string", () => {
    expect(toolCallPlanArgs(call({ plan: "P" }))).toBeNull();
    expect(toolCallPlanArgs(call({ title: "T" }))).toBeNull();
    expect(toolCallPlanArgs(call({ title: 42, plan: "P" }))).toBeNull();
  });

  it("returns null for malformed arguments JSON", () => {
    const toolCall: ToolCall = { id: "c1", type: "function", function: { name: "present_plan", arguments: "{not json" } };
    expect(toolCallPlanArgs(toolCall)).toBeNull();
  });
});

describe("selectTurnRunning (what PlanCard's Approve-disabled state reads)", () => {
  // `PlanCard.tsx` disables its "Approve & start acting" button via
  // `useSessionStore(selectTurnRunning(sessionId))`. This repo's vitest
  // config runs under a plain Node environment and only collects
  // `src/**/*.test.ts` (see vitest.config.ts) — there is no jsdom/React
  // Testing Library set up anywhere, so no component in this codebase is
  // rendered in a test. Rather than introduce a new testing subsystem for
  // this one component, this test pins the exact selector/state transition
  // PlanCard's `disabled` prop depends on instead.
  it("reports true only while markTurnRunning(sessionId, true) is in effect", () => {
    const sessionId = "session-plan-card-approve-gating";
    const selector = selectTurnRunning(sessionId);

    expect(selector(useSessionStore.getState())).toBe(false);

    useSessionStore.getState().markTurnRunning(sessionId, true);
    expect(selector(useSessionStore.getState())).toBe(true);

    useSessionStore.getState().markTurnRunning(sessionId, false);
    expect(selector(useSessionStore.getState())).toBe(false);
  });
});

describe("runVerificationPhase", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    useSettingsStore.setState({ verifyEnabled: true });
    usePermissionStore.setState({ mode: "manual" });
    useSessionStore.setState({ runningVerifyLabel: {} });
  });

  it("no-ops without any IPC calls when verifyEnabled is off (report-only posture stays off by default)", async () => {
    useSettingsStore.setState({ verifyEnabled: false });
    const addMessage = vi.fn();

    const failure = await runVerificationPhase("session-1", "turn-1", addMessage);

    expect(failure).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    expect(addMessage).not.toHaveBeenCalled();
  });

  it("never runs verification in plan mode, even with mutated files and verifyEnabled on", async () => {
    usePermissionStore.setState({ mode: "plan" });
    const addMessage = vi.fn();

    const failure = await runVerificationPhase("session-1", "turn-1", addMessage);

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
    const failure = await runVerificationPhase("session-1", "turn-1", addMessage);

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
    const failure = await runVerificationPhase("session-1", "turn-1", addMessage);

    expect(failure).toEqual({ label: "Tests", code: 1, output: "1 failing" });
    const notice = parseVerifyNotice(addMessage.mock.calls[0][0] as ChatMessage);
    expect(notice?.ok).toBe(false);
  });

  it("sets the running-verify-label for the duration of each command and clears it afterwards", async () => {
    let labelWhileRunning: string | null = null;
    invokeMock.mockResolvedValueOnce({
      commands: [{ id: "cmd-1", label: "Tests", command: "pnpm test", kind: "test", enabled: true }],
    }); // verify_get_config
    invokeMock.mockImplementationOnce(async () => {
      // Captured mid-flight — `verify_run` resolves after the label has
      // already been set, mirroring the "running <label>…" timeline row a
      // real (possibly long-running) command would show.
      labelWhileRunning = selectRunningVerifyLabel("session-1")(useSessionStore.getState());
      return {
        commandId: "cmd-1",
        label: "Tests",
        kind: "test",
        code: 0,
        stdout: "ok",
        stderr: "",
        durationMs: 10,
        timedOut: false,
      };
    });

    await runVerificationPhase("session-1", "turn-1", vi.fn());

    expect(labelWhileRunning).toBe("Tests");
    expect(selectRunningVerifyLabel("session-1")(useSessionStore.getState())).toBeNull();
  });

  it("clears the running-verify-label even when verify_run itself rejects", async () => {
    invokeMock.mockResolvedValueOnce({
      commands: [{ id: "cmd-1", label: "Tests", command: "pnpm test", kind: "test", enabled: true }],
    }); // verify_get_config
    invokeMock.mockRejectedValueOnce(new Error("command not found")); // verify_run

    await runVerificationPhase("session-1", "turn-1", vi.fn());

    expect(selectRunningVerifyLabel("session-1")(useSessionStore.getState())).toBeNull();
  });

  it("returns null without any IPC calls when the signal is already aborted", async () => {
    const controller = new AbortController();
    controller.abort();
    const addMessage = vi.fn();

    const failure = await runVerificationPhase("session-1", "turn-1", addMessage, controller.signal);

    expect(failure).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    expect(addMessage).not.toHaveBeenCalled();
  });

  it("cancels the in-flight command via tools_cancel_running and stops the phase when Stop fires mid-command", async () => {
    const controller = new AbortController();
    invokeMock.mockResolvedValueOnce({
      commands: [
        { id: "cmd-1", label: "Lint", command: "pnpm lint", kind: "lint", enabled: true },
        { id: "cmd-2", label: "Tests", command: "pnpm test", kind: "test", enabled: true },
      ],
    }); // verify_get_config
    let resolveVerifyRun: (value: unknown) => void = () => {};
    invokeMock.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveVerifyRun = resolve;
        })
    ); // verify_run for cmd-1 — deliberately left pending until after abort
    invokeMock.mockResolvedValueOnce(undefined); // tools_cancel_running

    const addMessage = vi.fn();
    const phasePromise = runVerificationPhase("session-1", "turn-1", addMessage, controller.signal);

    // Let the phase reach and start awaiting the (still-pending) verify_run
    // call before firing Stop, mirroring the user clicking Stop while a
    // command is genuinely running.
    await Promise.resolve();
    await Promise.resolve();
    controller.abort();

    const failure = await phasePromise;

    expect(failure).toBeNull();
    expect(addMessage).not.toHaveBeenCalled();
    // config + verify_run(cmd-1) + tools_cancel_running — cmd-2 must never
    // start once Stop fired mid-cmd-1.
    expect(invokeMock).toHaveBeenCalledTimes(3);
    expect(invokeMock).toHaveBeenNthCalledWith(3, "tools_cancel_running", { turnId: "turn-1" });

    // Let the abandoned invocation settle so it can't outlive the test.
    resolveVerifyRun({});
  });

  it("doesn't start a second configured command once Stop fires after the first one already finished", async () => {
    const controller = new AbortController();
    invokeMock.mockResolvedValueOnce({
      commands: [
        { id: "cmd-1", label: "Lint", command: "pnpm lint", kind: "lint", enabled: true },
        { id: "cmd-2", label: "Tests", command: "pnpm test", kind: "test", enabled: true },
      ],
    }); // verify_get_config
    invokeMock.mockImplementationOnce(async () => {
      // Stop fires while cmd-1 is nominally "in flight" but resolves
      // normally anyway (e.g. it finished just as the user clicked Stop).
      controller.abort();
      return {
        commandId: "cmd-1",
        label: "Lint",
        kind: "lint",
        code: 0,
        stdout: "ok",
        stderr: "",
        durationMs: 5,
        timedOut: false,
      };
    }); // verify_run for cmd-1

    const addMessage = vi.fn();
    const failure = await runVerificationPhase("session-1", "turn-1", addMessage, controller.signal);

    expect(failure).toBeNull();
    expect(addMessage).toHaveBeenCalledTimes(1); // cmd-1's passing notice only
    expect(invokeMock).toHaveBeenCalledTimes(2); // config + verify_run(cmd-1) — cmd-2 never runs
  });
});

describe("VERIFY_FIX_NOTE_PREFIX / isVerifyFixNotice", () => {
  it("is recognized by isVerifyFixNotice but never by isVerifyNotice, so MessageList never tries to JSON-parse it", () => {
    // This is deliberately plain prose, not a VerifyNotice JSON payload —
    // reusing VERIFY_NOTE_PREFIX for this message used to make
    // isVerifyNotice match, parseVerifyNotice fail, and MessageList drop it
    // from the timeline entirely (see the doc comment on
    // VERIFY_FIX_NOTE_PREFIX).
    const message: ChatMessage = {
      role: "system",
      content: `${VERIFY_FIX_NOTE_PREFIX} The verification command "Tests" failed (exit 1). Fix the reported problems, then stop.\n1 failing`,
    };

    expect(isVerifyFixNotice(message)).toBe(true);
    expect(isVerifyNotice(message)).toBe(false);
    expect(parseVerifyNotice(message)).toBeNull();
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

describe("maybeAutoPreviewNewestArtifact", () => {
  const sessionId = "artifact-preview-session";

  function withMessages(messages: ChatMessage[]): void {
    const session: ChatSession = {
      id: sessionId,
      title: "Test",
      messages,
      createdAt: 0,
      updatedAt: 0,
      pinned: false,
      unread: false,
      archived: false,
      groupId: null,
      workspacePath: null,
      personaId: null,
      attachedStackIds: [],
      docChatMode: false,
    };
    useSessionStore.setState((state) => ({
      sessions: [...state.sessions.filter((s) => s.id !== sessionId), session],
    }));
  }

  beforeEach(() => {
    useSettingsStore.setState({ artifactAutoPreview: true });
    useArtifactStore.getState().close();
    withMessages([]);
  });

  it("does nothing when artifactAutoPreview is off", () => {
    useSettingsStore.setState({ artifactAutoPreview: false });
    withMessages([{ role: "assistant", content: "```html\n<div>hi</div>\n```" }]);
    maybeAutoPreviewNewestArtifact(sessionId, 0);
    expect(useArtifactStore.getState().active).toBeNull();
  });

  it("does nothing when the turn produced no previewable artifact", () => {
    withMessages([{ role: "assistant", content: "plain text answer, no fences" }]);
    maybeAutoPreviewNewestArtifact(sessionId, 0);
    expect(useArtifactStore.getState().active).toBeNull();
  });

  it("opens the newest artifact produced by this turn", () => {
    withMessages([
      { role: "assistant", content: "```html\n<div>first</div>\n```" },
      { role: "assistant", content: "```html\n<div>second</div>\n```" },
    ]);
    maybeAutoPreviewNewestArtifact(sessionId, 0);
    expect(useArtifactStore.getState().active).toMatchObject({ sessionId, ref: { messageIndex: 1, blockIndex: 0 } });
  });

  it("never opens an artifact that predates anchorIndex (from an earlier turn)", () => {
    withMessages([
      { role: "assistant", content: "```html\n<div>earlier turn</div>\n```" },
      { role: "user", content: "do it again" },
      { role: "assistant", content: "plain text answer, no fences this time" },
    ]);
    // anchorIndex is the length of the transcript just before THIS turn's
    // user message was added (index 1 here) — this turn's own assistant
    // reply (index 2) has no fence, so nothing should open even though an
    // earlier turn's artifact still resolves fine via extractArtifacts.
    maybeAutoPreviewNewestArtifact(sessionId, 1);
    expect(useArtifactStore.getState().active).toBeNull();
  });

  it("does not steal the shared pane from a different session's already-open artifact", () => {
    // Reproduces the split-pane review finding: session A's artifact is
    // already open (e.g. the user is reading it), and a completely
    // different session B's turn finishes in the background (the split
    // pane) and would otherwise auto-open its own artifact into the SAME
    // shared pane, silently discarding whatever the user was looking at.
    const otherSessionId = "other-session";
    withMessages([{ role: "assistant", content: "```html\n<div>session B's page</div>\n```" }]);
    useArtifactStore.getState().open("session-a", { messageIndex: 0, blockIndex: 0, fingerprint: "whatever" });

    maybeAutoPreviewNewestArtifact(otherSessionId, 0);

    expect(useArtifactStore.getState().active?.sessionId).toBe("session-a");
  });

  it("still auto-opens into an empty pane when nothing is currently shown", () => {
    withMessages([{ role: "assistant", content: "```html\n<div>hi</div>\n```" }]);
    expect(useArtifactStore.getState().active).toBeNull();

    maybeAutoPreviewNewestArtifact(sessionId, 0);

    expect(useArtifactStore.getState().active?.sessionId).toBe(sessionId);
  });

  it("still refreshes the pane for a NEWER artifact in the SAME session it's already showing", () => {
    withMessages([{ role: "assistant", content: "```html\n<div>first</div>\n```" }]);
    maybeAutoPreviewNewestArtifact(sessionId, 0);
    expect(useArtifactStore.getState().active?.ref.messageIndex).toBe(0);

    withMessages([
      { role: "assistant", content: "```html\n<div>first</div>\n```" },
      { role: "assistant", content: "```html\n<div>second, same session</div>\n```" },
    ]);
    maybeAutoPreviewNewestArtifact(sessionId, 0);

    expect(useArtifactStore.getState().active?.ref.messageIndex).toBe(1);
  });
});
