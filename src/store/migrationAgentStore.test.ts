import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const agent = vi.hoisted(() => ({
  generateMigrationPlan: vi.fn(),
  fallbackHeuristicPlan: vi.fn(),
}));

const runner = vi.hoisted(() => ({
  runMigrationSliceAgent: vi.fn(),
}));

vi.mock("../lib/migrationAgent", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/migrationAgent")>()),
  ...agent,
}));

vi.mock("../lib/migrationAgentRunner", () => runner);

import type { MigrationPlan } from "../lib/migrationAgent";
import {
  __resetMigrationAgentControllersForTests,
  isTerminalMigrationRunStatus,
  useMigrationAgentStore,
} from "./migrationAgentStore";

function fixturePlan(overrides: Partial<MigrationPlan> = {}): MigrationPlan {
  return {
    goal: "Upgrade React to v19",
    summary: "Two slices to bump React safely.",
    usedFallback: false,
    createdAtMs: 1,
    slices: [
      {
        id: "slice-1",
        order: 1,
        title: "Bump the dependency",
        description: "Update package.json to react@19",
        riskLevel: "medium",
        riskNotes: ["Peer dependency conflicts possible"],
        rollbackNotes: "Revert package.json",
        filesLikely: ["package.json"],
      },
      {
        id: "slice-2",
        order: 2,
        title: "Fix call sites",
        description: "Update call sites for the new API",
        riskLevel: "high",
        riskNotes: ["Behavioral differences"],
        rollbackNotes: "Revert this slice's commits",
        filesLikely: [],
      },
    ],
    ...overrides,
  };
}

beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
  if (!("randomUUID" in globalThis.crypto)) {
    let counter = 0;
    Object.defineProperty(globalThis.crypto, "randomUUID", {
      configurable: true,
      value: () => `uuid-${++counter}`,
    });
  }
});

beforeEach(() => {
  localStorage.clear();
  agent.generateMigrationPlan.mockReset();
  agent.fallbackHeuristicPlan.mockReset();
  runner.runMigrationSliceAgent.mockReset();
  __resetMigrationAgentControllersForTests();
  useMigrationAgentStore.setState({
    runs: [],
    selectedRunId: null,
    activityByRun: {},
    busy: {},
    error: null,
    notice: null,
  });
});

describe("migrationAgentStore", () => {
  it("creates a run and generates a plan", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    expect(run.status).toBe("planned");
    expect(run.plan?.slices).toHaveLength(2);
    expect(useMigrationAgentStore.getState().runs).toHaveLength(1);
    expect(useMigrationAgentStore.getState().selectedRunId).toBe(run.runId);
  });

  it("falls back to a heuristic plan and records the error when plan generation fails", async () => {
    agent.generateMigrationPlan.mockRejectedValue(new Error("no model available"));
    agent.fallbackHeuristicPlan.mockReturnValue(fixturePlan({ usedFallback: true }));
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    expect(run.status).toBe("planned");
    expect(run.plan?.usedFallback).toBe(true);
    expect(run.error).toContain("no model available");
  });

  it("persists runs to localStorage across store instances", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    expect(localStorage.getItem("little-monkey-migration-agent-runs-v1")).toBeTruthy();

    useMigrationAgentStore.setState({ runs: [], selectedRunId: null, activityByRun: {}, busy: {}, error: null, notice: null });
    useMigrationAgentStore.getState().init();
    expect(useMigrationAgentStore.getState().runs).toHaveLength(1);
  });

  it("attaches a worktree and attempts the first slice, moving to awaiting_push on success", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    useMigrationAgentStore.getState().attachWorktree(run.runId, "wt-1", "codex/migration/abc", "wt-1");

    runner.runMigrationSliceAgent.mockResolvedValue({
      outcome: "completed",
      summary: "Bumped react to 19.0.0 and ran the test suite: all green.",
      durableRunId: "durable-1",
    });

    await useMigrationAgentStore.getState().attemptFirstSlice(run.runId);

    const updated = useMigrationAgentStore.getState().runs.find((r) => r.runId === run.runId);
    expect(updated?.status).toBe("awaiting_push");
    expect(updated?.sliceOutcome?.outcome).toBe("completed");
    expect(updated?.sliceOutcome?.durableRunId).toBe("durable-1");
  });

  it("marks the run failed when the slice agent errors", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    useMigrationAgentStore.getState().attachWorktree(run.runId, "wt-1", "codex/migration/abc", "wt-1");

    runner.runMigrationSliceAgent.mockResolvedValue({
      outcome: "error",
      summary: "Stopped after reaching the safety limit.",
      durableRunId: null,
    });

    await useMigrationAgentStore.getState().attemptFirstSlice(run.runId);
    const updated = useMigrationAgentStore.getState().runs.find((r) => r.runId === run.runId);
    expect(updated?.status).toBe("failed");
    expect(updated?.error).toContain("safety limit");
  });

  it("refuses to attempt a slice before a worktree is attached", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    await expect(useMigrationAgentStore.getState().attemptFirstSlice(run.runId)).rejects.toThrow();
    expect(runner.runMigrationSliceAgent).not.toHaveBeenCalled();
  });

  it("marks a run completed once a PR is opened", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    useMigrationAgentStore.getState().markPrOpened(run.runId, 42, "https://github.com/owner/repo/pull/42");
    const updated = useMigrationAgentStore.getState().runs.find((r) => r.runId === run.runId);
    expect(updated?.status).toBe("completed");
    expect(updated?.prNumber).toBe(42);
    expect(isTerminalMigrationRunStatus(updated!.status)).toBe(true);
  });

  it("deletes a run", async () => {
    agent.generateMigrationPlan.mockResolvedValue(fixturePlan());
    const run = await useMigrationAgentStore.getState().createRun("Upgrade React to v19", "owner/repo");
    useMigrationAgentStore.getState().deleteRun(run.runId);
    expect(useMigrationAgentStore.getState().runs).toHaveLength(0);
  });
});
