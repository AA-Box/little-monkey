import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

import {
  collectUserPromptSubmitContext,
  evaluatePreToolUseHooks,
  fireObservedHooks,
  matcherMatches,
  type HookExecOutcome,
} from "./userHooks";
import { parseHooksConfig, useUserHooksStore, type UserHookDef } from "../store/userHooksStore";

function hook(overrides: Partial<UserHookDef> = {}): UserHookDef {
  return { id: crypto.randomUUID(), event: "PreToolUse", command: "check.sh", ...overrides };
}

function outcome(overrides: Partial<HookExecOutcome> = {}): HookExecOutcome {
  return { exit_code: 0, stdout: "", stderr: "", timed_out: false, ...overrides };
}

beforeEach(() => {
  invokeMock.mockReset();
  useUserHooksStore.setState({ hooks: [], loaded: true });
});

describe("matcherMatches", () => {
  it("matches every tool when no matcher is set", () => {
    expect(matcherMatches(undefined, "write_file")).toBe(true);
    expect(matcherMatches("  ", "grep")).toBe(true);
  });

  it("treats the matcher as an anchored regex", () => {
    expect(matcherMatches("write_file|edit_file", "write_file")).toBe(true);
    expect(matcherMatches("write_file|edit_file", "edit_file")).toBe(true);
    expect(matcherMatches("write_file|edit_file", "grep")).toBe(false);
    // Anchored: a substring match is not enough.
    expect(matcherMatches("write", "write_file")).toBe(false);
    expect(matcherMatches("write.*", "write_file")).toBe(true);
  });

  it("falls back to exact-name equality for an invalid regex", () => {
    expect(matcherMatches("write_file(", "write_file(")).toBe(true);
    expect(matcherMatches("write_file(", "write_file")).toBe(false);
  });
});

describe("evaluatePreToolUseHooks", () => {
  it("denies with the stderr reason when a hook exits non-zero", async () => {
    useUserHooksStore.setState({ hooks: [hook()] });
    invokeMock.mockResolvedValue(outcome({ exit_code: 2, stderr: "protected path\n" }));

    const denial = await evaluatePreToolUseHooks("write_file", { path: "a.txt" }, "sess-1");

    expect(denial).toEqual({ reason: "protected path" });
  });

  it("denies when an exit-0 hook prints {\"decision\":\"deny\"} on stdout", async () => {
    useUserHooksStore.setState({ hooks: [hook()] });
    invokeMock.mockResolvedValue(outcome({ stdout: JSON.stringify({ decision: "deny", reason: "not on Fridays" }) }));

    const denial = await evaluatePreToolUseHooks("run_shell", {}, undefined);

    expect(denial).toEqual({ reason: "not on Fridays" });
  });

  it("allows on a clean exit-0 run, passing the payload on stdin", async () => {
    useUserHooksStore.setState({ hooks: [hook({ command: "audit.sh" })] });
    invokeMock.mockResolvedValue(outcome({ stdout: "looks fine" }));

    const denial = await evaluatePreToolUseHooks("write_file", { path: "a.txt" }, "sess-9");

    expect(denial).toBeNull();
    const [command, args] = invokeMock.mock.calls[0];
    expect(command).toBe("hook_exec");
    const payload = JSON.parse((args as { payload: string }).payload) as Record<string, unknown>;
    expect(payload).toEqual({ event: "PreToolUse", tool_name: "write_file", args: { path: "a.txt" }, session_id: "sess-9" });
  });

  it("proceeds (WARN, not deny) when the hook times out or was killed", async () => {
    useUserHooksStore.setState({ hooks: [hook()] });
    invokeMock.mockResolvedValue(outcome({ exit_code: null, timed_out: true }));

    expect(await evaluatePreToolUseHooks("write_file", {}, undefined)).toBeNull();
  });

  it("proceeds when hook execution itself fails (spawn error)", async () => {
    useUserHooksStore.setState({ hooks: [hook()] });
    invokeMock.mockRejectedValue(new Error("Failed to spawn hook"));

    expect(await evaluatePreToolUseHooks("write_file", {}, undefined)).toBeNull();
  });

  it("only runs hooks whose matcher covers the tool name", async () => {
    useUserHooksStore.setState({ hooks: [hook({ matcher: "write_file|edit_file" })] });
    invokeMock.mockResolvedValue(outcome({ exit_code: 1, stderr: "denied" }));

    expect(await evaluatePreToolUseHooks("grep", {}, undefined)).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();

    expect(await evaluatePreToolUseHooks("edit_file", {}, undefined)).toEqual({ reason: "denied" });
  });

  it("returns the FIRST denial in configured order and stops evaluating", async () => {
    useUserHooksStore.setState({ hooks: [hook({ command: "first.sh" }), hook({ command: "second.sh" })] });
    invokeMock.mockResolvedValueOnce(outcome({ exit_code: 1, stderr: "first says no" }));

    const denial = await evaluatePreToolUseHooks("write_file", {}, undefined);

    expect(denial).toEqual({ reason: "first says no" });
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });
});

describe("collectUserPromptSubmitContext", () => {
  it("joins the non-empty stdouts of clean runs and skips failing hooks", async () => {
    useUserHooksStore.setState({
      hooks: [
        hook({ event: "UserPromptSubmit", command: "ctx-a.sh" }),
        hook({ event: "UserPromptSubmit", command: "broken.sh" }),
        hook({ event: "UserPromptSubmit", command: "ctx-b.sh" }),
      ],
    });
    invokeMock
      .mockResolvedValueOnce(outcome({ stdout: "## Branch status\nclean\n" }))
      .mockResolvedValueOnce(outcome({ exit_code: 1, stderr: "boom" }))
      .mockResolvedValueOnce(outcome({ stdout: "reminder: deploy freeze" }));

    const context = await collectUserPromptSubmitContext("sess-1");

    expect(context).toBe("## Branch status\nclean\n\nreminder: deploy freeze");
  });

  it("returns an empty string when no hooks are configured, without invoking anything", async () => {
    expect(await collectUserPromptSubmitContext("sess-1")).toBe("");
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("fireObservedHooks", () => {
  it("fires every matching hook and swallows failures", async () => {
    useUserHooksStore.setState({
      hooks: [hook({ event: "PostToolUse", command: "log.sh" }), hook({ event: "PostToolUse", command: "broken.sh" })],
    });
    invokeMock.mockResolvedValueOnce(outcome()).mockRejectedValueOnce(new Error("spawn failed"));

    fireObservedHooks("PostToolUse", { tool_name: "write_file", result: "ok" });
    await vi.waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));

    const payload = JSON.parse((invokeMock.mock.calls[0][1] as { payload: string }).payload) as Record<string, unknown>;
    expect(payload.event).toBe("PostToolUse");
    expect(payload.result).toBe("ok");
  });
});

describe("parseHooksConfig", () => {
  it("keeps valid entries and drops malformed ones", () => {
    const raw = JSON.stringify([
      { id: "a", event: "PreToolUse", command: "check.sh", matcher: "write_file" },
      { event: "SessionStart", command: "notify.sh" },
      { event: "NotAnEvent", command: "x.sh" },
      { event: "PreToolUse", command: "   " },
      "garbage",
    ]);

    const parsed = parseHooksConfig(raw);

    expect(parsed).toHaveLength(2);
    expect(parsed[0]).toMatchObject({ id: "a", event: "PreToolUse", command: "check.sh", matcher: "write_file" });
    expect(parsed[1]).toMatchObject({ event: "SessionStart", command: "notify.sh" });
    expect(parsed[1].id.length).toBeGreaterThan(0);
  });

  it("returns empty for blank, non-array, or corrupt content", () => {
    expect(parseHooksConfig("")).toEqual([]);
    expect(parseHooksConfig("{}")).toEqual([]);
    expect(parseHooksConfig("not json")).toEqual([]);
  });
});
