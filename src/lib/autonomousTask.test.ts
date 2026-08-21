import { describe, expect, it } from "vitest";

import {
  canRunTaskNodesTogether,
  createAutonomousTask,
  createTaskPlan,
  getReadyTaskPlanNodes,
  hasAuthoritativeAcceptanceEvidence,
  validateTaskPlan,
  type AutonomousTask,
} from "./autonomousTask";
import { runAutonomousTask, type AutonomousTaskRuntime } from "./autonomousTaskRunner";
import { AUTONOMOUS_TASK_EVAL_FIXTURES, evaluateAutonomousTaskRouting } from "./autonomousTaskEval";

const target = { kind: "provider", key: "provider:test:model", label: "test", displayName: "test", providerId: "test", endpoint: "https://example.test", model: "model", credentialRefId: "credential:test", capabilities: { toolCalling: { state: "yes", evidence: "test" }, vision: { state: "unknown", evidence: "test" } }, availability: { status: "available", evidence: "test" } } as never;
const roots = [{ id: "root", path: "/tmp/task-test", label: "workspace", is_primary: true }];
const permissions = { mode: "auto", unattended: true, allowNetwork: false, allowExternalMutations: false };

function task(strategy: "DIRECT" | "PARALLEL_DELEGATE" = "DIRECT"): AutonomousTask {
  return createAutonomousTask({ objective: "implement the requested change", sessionId: "session", targetSnapshot: target, workspaceRoots: roots, permissionSnapshot: permissions, constraints: { strategy, allowParallel: strategy === "PARALLEL_DELEGATE" } });
}

function evidence(taskValue: AutonomousTask, criterionIndex: number, id: string) {
  return { evidenceId: id, criterionId: taskValue.acceptanceCriteria[criterionIndex].id, name: id, passed: true, authoritative: true, stale: false, summary: "passed", exitCode: 0, durationMs: 1, createdAtMs: Date.now() };
}

describe("autonomous task planning", () => {
  it("keeps the routing evaluation corpus green", () => {
    expect(AUTONOMOUS_TASK_EVAL_FIXTURES.length).toBeGreaterThanOrEqual(15);
    expect(evaluateAutonomousTaskRouting().every((result) => result.passed)).toBe(true);
  });

  it("selects direct and parallel plans with dependency-safe readiness", () => {
    const direct = createTaskPlan("simple rename", "DIRECT");
    expect(validateTaskPlan(direct)).toEqual([]);
    expect(getReadyTaskPlanNodes(direct).map((node) => node.nodeId)).toEqual(["implement"]);
    const parallel = createTaskPlan("parallel independent changes", "PARALLEL_DELEGATE", 3);
    expect(validateTaskPlan(parallel)).toEqual([]);
    expect(parallel.nodes.filter((node) => node.taskClass === "implementation")).toHaveLength(3);
    expect(canRunTaskNodesTogether(parallel.nodes[1], parallel.nodes[2])).toBe(true);
    expect(canRunTaskNodesTogether({ ...parallel.nodes[1], mutationScope: ["same"] }, { ...parallel.nodes[2], mutationScope: ["same"] })).toBe(false);
  });

  it("rejects missing dependencies and cycles", () => {
    const plan = createTaskPlan("x", "DIRECT");
    const bad = { ...plan, nodes: [{ ...plan.nodes[0], dependencies: ["missing"] }, { ...plan.nodes[1], dependencies: [plan.nodes[2].nodeId] }, { ...plan.nodes[2], dependencies: [plan.nodes[1].nodeId] }] };
    expect(validateTaskPlan(bad)).toEqual(expect.arrayContaining(["implement depends on missing node missing", expect.stringContaining("cycle involving")]));
  });
});

describe("autonomous task execution", () => {
  it("runs a bounded plan, records authoritative evidence, and completes", async () => {
    const initial = task();
    const runtime: AutonomousTaskRuntime = {
      executeNode: async (current, node) => ({ ok: true, summary: `${node.nodeId} complete`, evidence: node.taskClass === "investigation" ? [evidence(current, 0, "objective-worker")] : undefined }),
      verify: async (current) => ({ ok: true, summary: "checks passed", evidence: [evidence(current, 2, "verification")] }),
      review: async (current) => ({ ok: true, summary: "review passed", evidence: [evidence(current, 1, "review")] }),
    };
    const events: string[] = [];
    const result = await runAutonomousTask({ task: initial, resolvedTarget: { kind: "provider", providerId: "test", model: "model" } as never, runtime, eventSink: (event) => { events.push(event.eventType); } });
    expect(result.outcome).toBe("SUCCEEDED");
    expect(hasAuthoritativeAcceptanceEvidence(result)).toBe(true);
    expect(events).toContain("task_completed");
  });

  it("executes independent worker nodes concurrently before integration", async () => {
    const initial = task("PARALLEL_DELEGATE");
    let active = 0;
    let maxActive = 0;
    const runtime: AutonomousTaskRuntime = {
      executeNode: async (_current, node) => {
        if (node.taskClass === "implementation") { active += 1; maxActive = Math.max(maxActive, active); await Promise.resolve(); active -= 1; }
        return { ok: true, summary: node.nodeId };
      },
      integrate: async () => ({ ok: true, summary: "integrated" }),
      verify: async (current) => ({ ok: true, summary: "verified", evidence: [evidence(current, 2, "verification")] }),
      review: async (current) => ({ ok: true, summary: "reviewed", evidence: [evidence(current, 1, "review")] }),
    };
    const result = await runAutonomousTask({ task: initial, resolvedTarget: { kind: "provider", providerId: "test", model: "model" } as never, runtime });
    expect(result.outcome).toBe("SUCCEEDED");
    expect(maxActive).toBeGreaterThan(1);
  });
});
