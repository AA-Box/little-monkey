import { describe, expect, it } from "vitest";

import type { ToolCall } from "../../lib/llamaClient";
import { protectToolResult } from "../../lib/untrustedContent";
import {
  activityCallDiff,
  activityCallSubject,
  activityProgress,
  capActivityText,
  formatActivityResult,
  groupAssistantRound,
  liveActivityLabel,
  summarizeActivity,
  type ActivityCall,
} from "./activityTimeline";

function call(id: string, name: string, args: Record<string, unknown> = {}): ToolCall {
  return {
    id,
    type: "function",
    function: { name, arguments: JSON.stringify(args) },
  };
}

function activity(name: string, args: Record<string, unknown> = {}, result = "ok"): ActivityCall {
  return {
    id: `${name}-${JSON.stringify(args)}`,
    name,
    args: JSON.stringify(args),
    result,
  };
}

describe("groupAssistantRound", () => {
  it("groups every non-task call into one activity entry while preserving call order", () => {
    const calls = [
      call("read", "read_file", { path: "src/a.ts" }),
      call("task-a", "task", { description: "Inspect API" }),
      call("shell", "run_shell", { command: "pnpm test" }),
      call("task-b", "task", { description: "Inspect UI" }),
      call("edit", "edit_file", { path: "src/a.ts" }),
    ];
    const results = new Map([
      ["read", "contents"],
      ["task-a", "report a"],
      ["shell", "passed"],
      ["task-b", "report b"],
    ]);

    expect(groupAssistantRound(calls, results)).toEqual([
      {
        kind: "activity",
        calls: [
          expect.objectContaining({ id: "read", name: "read_file", result: "contents" }),
          expect.objectContaining({ id: "shell", name: "run_shell", result: "passed" }),
          expect.objectContaining({ id: "edit", name: "edit_file", result: undefined }),
        ],
      },
      {
        kind: "tasks",
        calls: [
          expect.objectContaining({ id: "task-a", result: "report a" }),
          expect.objectContaining({ id: "task-b", result: "report b" }),
        ],
      },
    ]);
  });

  it("places the task group first when delegation was the round's first action", () => {
    const entries = groupAssistantRound(
      [call("task", "task"), call("read", "read_file")],
      new Map(),
    );
    expect(entries.map((entry) => entry.kind)).toEqual(["tasks", "activity"]);
  });
});

describe("summarizeActivity", () => {
  it("writes a compact human summary in the original action order", () => {
    const calls = [
      activity("read_file", { path: "a.ts" }),
      activity("read_file", { path: "b.ts" }),
      activity("read_file", { path: "c.ts" }),
      activity("run_shell", { command: "pnpm test" }),
      activity("edit_file", { path: "a.ts" }),
      activity("write_file", { path: "b.ts" }),
    ];
    expect(summarizeActivity(calls)).toBe("Read 3 files, ran a command, edited 2 files");
  });

  it("does not merge actions that happened on opposite sides of another action", () => {
    const calls = [
      activity("read_file"),
      activity("run_shell"),
      activity("read_file"),
    ];
    expect(summarizeActivity(calls)).toBe("Read a file, ran a command, read a file");
  });

  it("never describes pending or failed file mutations as completed edits", () => {
    const calls: ActivityCall[] = [
      activity("edit_file", { path: "done.ts" }),
      activity("edit_file", { path: "failed.ts" }, JSON.stringify({ error: "denied" })),
      { ...activity("write_file", { path: "pending.ts" }), result: undefined },
    ];
    expect(summarizeActivity(calls)).toBe(
      "Edited a file, attempted a file edit, proposed a file edit",
    );
  });
});

describe("activityProgress", () => {
  it("keeps both running and failed state visible for a partially settled round", () => {
    const progress = activityProgress([
      activity("read_file", {}, JSON.stringify({ error: "denied" })),
      { ...activity("edit_file"), result: undefined },
    ]);
    expect(progress).toMatchObject({
      status: "running",
      label: "Running · 1 failed",
      pendingCount: 1,
      failedCount: 1,
    });
  });

  it("reports a settled failure and a settled success with text labels", () => {
    expect(activityProgress([activity("read_file", {}, JSON.stringify({ error: "missing" }))])).toMatchObject({
      status: "failed",
      label: "Failed",
    });
    expect(activityProgress([activity("read_file")])).toMatchObject({
      status: "completed",
      label: "Completed",
    });
  });

  it("recognizes errors inside persisted untrusted-data envelopes", () => {
    const protectedError = protectToolResult(
      "run_shell",
      JSON.stringify({ error: "Permission denied" }),
    );

    expect(activityProgress([activity("run_shell", {}, protectedError)])).toMatchObject({
      status: "failed",
      label: "Failed",
    });
    expect(formatActivityResult(protectedError).text).toBe(
      '{\n  "error": "Permission denied"\n}',
    );
  });
});

describe("activity detail helpers", () => {
  it("shows both a shell command and its working directory", () => {
    expect(activityCallSubject(activity("run_shell", { command: "pnpm test", cwd: "desktop" }))).toBe(
      "pnpm test\nWorking directory: desktop",
    );
  });

  it("derives bounded before/after previews for edits", () => {
    const diff = activityCallDiff(
      activity("edit_file", {
        path: "src/a.ts",
        old_string: "before",
        new_string: "after",
      }),
    );
    expect(diff).toEqual({
      kind: "edit",
      state: "applied",
      before: { text: "before", truncated: false },
      after: { text: "after", truncated: false },
    });
  });

  it("marks failed and pending mutation previews as attempted or proposed", () => {
    const args = {
      path: "src/a.ts",
      old_string: "before",
      new_string: "after",
    };
    expect(
      activityCallDiff(activity("edit_file", args, JSON.stringify({ error: "denied" })))?.state,
    ).toBe("attempted");
    expect(
      activityCallDiff({ ...activity("edit_file", args), result: undefined })?.state,
    ).toBe("proposed");
  });

  it("derives an added-content preview for writes and caps large payloads", () => {
    const diff = activityCallDiff(
      activity("write_file", { path: "src/a.ts", content: "x".repeat(3_000) }),
    );
    expect(diff?.kind).toBe("write");
    expect(diff?.after.truncated).toBe(true);
    expect(diff?.after.text).toContain("… truncated");
    expect(diff?.after.text.length).toBeLessThan(2_500);
  });

  it("caps result output by lines and formats JSON first", () => {
    expect(capActivityText(["a", "b", "c"].join("\n"), 100, 2)).toEqual({
      text: "a\nb\n… truncated",
      truncated: true,
    });
    expect(formatActivityResult(JSON.stringify({ ok: true })).text).toBe('{\n  "ok": true\n}');
  });

  it("uses generic operational live labels rather than model reasoning", () => {
    expect(liveActivityLabel("read_file")).toBe("Reading files");
    expect(liveActivityLabel("edit_file")).toBe("Editing files");
    expect(liveActivityLabel("run_shell")).toBe("Running a command");
  });
});
