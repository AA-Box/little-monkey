import { beforeEach, describe, expect, it } from "vitest";

import { selectDebateRuns, useDebateStore, type DebatePosition } from "./debateStore";

function pendingPositions(): DebatePosition[] {
  return (["proposer", "critic", "security", "reliability", "cost", "user_advocate"] as const).map((roleId) => ({
    roleId,
    roleLabel: roleId,
    status: "pending",
    position: null,
    objections: [],
    rawOutput: "",
    error: null,
    startedAt: null,
    completedAt: null,
  }));
}

beforeEach(() => {
  useDebateStore.setState({ runs: {}, order: [], activeRunId: null });
});

describe("debateStore", () => {
  it("creates a run with the given positions, selects it, and orders newest first", () => {
    const first = useDebateStore.getState().create("run-1", "Question one?", pendingPositions());
    const second = useDebateStore.getState().create("run-2", "Question two?", pendingPositions());

    expect(first.status).toBe("idle");
    expect(first.positions).toHaveLength(6);
    expect(useDebateStore.getState().activeRunId).toBe("run-2");
    expect(selectDebateRuns(useDebateStore.getState()).map((r) => r.id)).toEqual(["run-2", "run-1"]);
    expect(second.question).toBe("Question two?");
  });

  it("marks a run running exactly once, freezing startedAt on the first call", async () => {
    useDebateStore.getState().create("run-1", "Q?", pendingPositions());
    useDebateStore.getState().markRunning("run-1");
    const startedAt = useDebateStore.getState().runs["run-1"].startedAt;
    expect(useDebateStore.getState().runs["run-1"].status).toBe("running");
    expect(startedAt).not.toBeNull();

    await new Promise((resolve) => setTimeout(resolve, 5));
    useDebateStore.getState().markRunning("run-1");
    expect(useDebateStore.getState().runs["run-1"].startedAt).toBe(startedAt);
  });

  it("patches only the targeted role's position, leaving siblings untouched", () => {
    useDebateStore.getState().create("run-1", "Q?", pendingPositions());
    useDebateStore.getState().updatePosition("run-1", "critic", {
      status: "completed",
      position: "Watch out for X.",
      objections: ["Risk A"],
    });

    const run = useDebateStore.getState().runs["run-1"];
    const critic = run.positions.find((p) => p.roleId === "critic")!;
    const proposer = run.positions.find((p) => p.roleId === "proposer")!;
    expect(critic.status).toBe("completed");
    expect(critic.position).toBe("Watch out for X.");
    expect(proposer.status).toBe("pending");
  });

  it("stores the synthesis and finish() computes durationMs from startedAt", async () => {
    useDebateStore.getState().create("run-1", "Q?", pendingPositions());
    useDebateStore.getState().markRunning("run-1");
    useDebateStore.getState().setSynthesis("run-1", {
      recommendation: "Do the thing.",
      objectionHandling: [],
      tradeoffs: "",
      whyThisWon: "",
      parseFailed: false,
      raw: "{}",
    });
    await new Promise((resolve) => setTimeout(resolve, 5));
    useDebateStore.getState().finish("run-1", "completed", null);

    const run = useDebateStore.getState().runs["run-1"];
    expect(run.status).toBe("completed");
    expect(run.synthesis?.recommendation).toBe("Do the thing.");
    expect(run.durationMs).not.toBeNull();
    expect(run.durationMs!).toBeGreaterThanOrEqual(0);
  });

  it("remove() drops the run, its order entry, and clears activeRunId if it pointed there", () => {
    useDebateStore.getState().create("run-1", "Q?", pendingPositions());
    useDebateStore.getState().remove("run-1");

    expect(useDebateStore.getState().runs["run-1"]).toBeUndefined();
    expect(useDebateStore.getState().order).toEqual([]);
    expect(useDebateStore.getState().activeRunId).toBeNull();
  });

  it("is a no-op when patching/finishing a run id that doesn't exist", () => {
    expect(() => useDebateStore.getState().markRunning("missing")).not.toThrow();
    expect(() => useDebateStore.getState().updatePosition("missing", "critic", { status: "completed" })).not.toThrow();
    expect(() => useDebateStore.getState().finish("missing", "completed", null)).not.toThrow();
    expect(useDebateStore.getState().runs).toEqual({});
  });
});
