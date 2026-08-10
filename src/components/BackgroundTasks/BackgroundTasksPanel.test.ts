// DOM-free logic tests for the Background-tasks drawer's agent grouping —
// same convention as `SubagentRow.test.ts` (vitest `node` environment, no
// rendering harness): the JSX isn't unit-tested, every piece of logic that
// determines what it renders is.
import { describe, expect, it } from "vitest";

import type { SubagentRun } from "../../store/subagentStore";
import { agentEntryRunning, groupAgentRuns, type AgentPanelEntry } from "./BackgroundTasksPanel";

function run(overrides: Partial<SubagentRun> & { taskId: string }): SubagentRun {
  return {
    sessionId: "s1",
    cancelId: `cancel-${overrides.taskId}`,
    description: `task ${overrides.taskId}`,
    profile: "explore",
    status: "done",
    startedAt: 1000,
    finishedAt: 2000,
    lastActivity: "",
    toolCallCount: 0,
    usage: undefined,
    liveMessages: [],
    ...overrides,
  };
}

describe("groupAgentRuns", () => {
  it("passes ungrouped runs through as singles, preserving order", () => {
    const runs = [run({ taskId: "a" }), run({ taskId: "b" })];
    expect(groupAgentRuns(runs)).toEqual([
      { kind: "single", run: runs[0] },
      { kind: "single", run: runs[1] },
    ]);
  });

  it("collapses same-groupId runs into one group at the first member's position", () => {
    const runs = [
      run({ taskId: "g1-a", groupId: "g1" }),
      run({ taskId: "lone" }),
      run({ taskId: "g1-b", groupId: "g1" }),
      run({ taskId: "g1-c", groupId: "g1" }),
    ];
    const entries = groupAgentRuns(runs);
    expect(entries).toHaveLength(2);
    expect(entries[0]).toEqual({ kind: "group", groupId: "g1", runs: [runs[0], runs[2], runs[3]] });
    expect(entries[1]).toEqual({ kind: "single", run: runs[1] });
  });

  it("keeps distinct groups apart", () => {
    const runs = [
      run({ taskId: "g1-a", groupId: "g1" }),
      run({ taskId: "g2-a", groupId: "g2" }),
      run({ taskId: "g1-b", groupId: "g1" }),
      run({ taskId: "g2-b", groupId: "g2" }),
    ];
    const entries = groupAgentRuns(runs);
    expect(entries.map((entry) => (entry.kind === "group" ? entry.groupId : "single"))).toEqual(["g1", "g2"]);
    expect(entries.every((entry) => entry.kind === "group" && entry.runs.length === 2)).toBe(true);
  });

  it("demotes a one-member group to a plain card", () => {
    // e.g. Clear dropped the group's other finished members.
    const only = run({ taskId: "g1-a", groupId: "g1" });
    expect(groupAgentRuns([only])).toEqual([{ kind: "single", run: only }]);
  });
});

describe("agentEntryRunning", () => {
  const doneRun = run({ taskId: "d" });
  const runningRun = run({ taskId: "r", status: "running", finishedAt: undefined });

  it("reports a single run's own status", () => {
    expect(agentEntryRunning({ kind: "single", run: runningRun })).toBe(true);
    expect(agentEntryRunning({ kind: "single", run: doneRun })).toBe(false);
  });

  it("keeps a group running until its last member settles", () => {
    const half: AgentPanelEntry = { kind: "group", groupId: "g", runs: [doneRun, runningRun] };
    const settled: AgentPanelEntry = { kind: "group", groupId: "g", runs: [doneRun, run({ taskId: "e", status: "error" })] };
    expect(agentEntryRunning(half)).toBe(true);
    expect(agentEntryRunning(settled)).toBe(false);
  });
});
