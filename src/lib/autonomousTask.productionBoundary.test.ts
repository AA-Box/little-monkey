import { afterEach, describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return { ...actual, invoke: vi.fn(async (command: string) => command === "verify_get_config" ? { commands: [] } : undefined) };
});
vi.mock("./subagent", () => ({
  runSubagentTask: vi.fn(async () => JSON.stringify({ verdict: "pass", findings: [], filesReviewed: ["allowed.ts"], acceptanceCriteria: [], securityFindings: [], testCoverageFindings: [] })),
  runSubagentTaskStructured: vi.fn(async () => ({ outcome: "done", report: "completed", worktree: undefined, changedFiles: [], usage: {} })),
}));
vi.mock("./agentWorktree", () => ({
  agentWorktreeClient: {
    workspaceRevision: vi.fn(async () => "r1"),
    workspaceSnapshot: vi.fn(async () => ({ id: "snapshot-1", revision: "r0", changed_files: [] })),
    workspaceChangedFilesSinceSnapshot: vi.fn(),
    workspaceRestorePaths: vi.fn(async () => undefined),
    workspaceSnapshotDiscard: vi.fn(async () => undefined),
    workspaceChangedFiles: vi.fn(async () => []),
  },
}));

import { createAutonomousTask, createTaskPlan, installTaskPlan } from "./autonomousTask";
import { buildAutonomousPlacementRunSpec, defaultAutonomousTaskRuntime, runAutonomousTask } from "./autonomousTaskRunner";
import { agentWorktreeClient } from "./agentWorktree";

const target = { kind: "provider", key: "provider:test:model", label: "test", displayName: "test", providerId: "test", endpoint: "https://example.test", model: "model", credentialRefId: "credential:test", capabilities: { toolCalling: { state: "yes", evidence: "test" }, vision: { state: "unknown", evidence: "test" } }, availability: { status: "available", evidence: "test" } } as never;
const roots = [{ id: "root", path: "/tmp", label: "workspace", is_primary: true }];
const permissions = { mode: "auto", unattended: true, allowNetwork: false, allowExternalMutations: false };

afterEach(() => vi.clearAllMocks());

describe("default autonomous runtime production boundary", () => {
  it("rolls back an out-of-scope shared mutation before bounded repair", async () => {
    const base = createAutonomousTask({ objective: "edit allowed.ts", targetSnapshot: target, workspaceRoots: roots, permissionSnapshot: permissions, constraints: { strategy: "DIRECT" }, planningContext: { relevantFiles: ["allowed.ts"] } });
    const plan = createTaskPlan("edit allowed.ts", "DIRECT", 1, base.planningContext);
    const initial = installTaskPlan(base, plan);
    vi.mocked(agentWorktreeClient.workspaceChangedFilesSinceSnapshot)
      .mockResolvedValueOnce(["allowed.ts", "secret.txt"])
      .mockResolvedValue([]);
    const result = await runAutonomousTask({ task: initial, resolvedTarget: target, runtime: defaultAutonomousTaskRuntime(target) });
    expect(result.outcome).toBe("WAITING_USER");
    expect(agentWorktreeClient.workspaceRestorePaths).toHaveBeenCalledWith("snapshot-1", ["secret.txt"]);
    expect(agentWorktreeClient.workspaceSnapshotDiscard).toHaveBeenCalled();
  });

  it.each(["remote_node", "docker"] as const)("consumes %s placement before the receiver process runs", async (kind) => {
    const base = createAutonomousTask({ objective: "edit allowed.ts", targetSnapshot: target, workspaceRoots: roots, permissionSnapshot: permissions, constraints: { strategy: "DIRECT" }, planningContext: { relevantFiles: ["allowed.ts"] } });
    const plan = createTaskPlan("edit allowed.ts", "DIRECT", 1, base.planningContext);
    const sourceNode = { ...plan.nodes.find((node) => node.taskClass === "implementation")!, executionPlacement: { kind, targetId: kind === "docker" ? "runner:latest" : "runner-1", nodeId: "implement", reason: "test placement" } };
    const spec = buildAutonomousPlacementRunSpec(installTaskPlan(base, plan), sourceNode, kind);
    const receiver = await promisify(execFile)(process.execPath, ["-e", "const spec=JSON.parse(process.argv[1]); const node=spec.autonomous_task.task_snapshot.plan.nodes[0]; if(node.executionPlacement.kind !== 'local' || node.executionPlacement.placementFulfilled !== true) process.exit(2); console.log(node.requestedExecutionPlacement.kind)", JSON.stringify(spec)]);
    expect(receiver.stdout.trim()).toBe(kind);
  });

  it("classifies a lost external target without scheduling a generic repair", async () => {
    const base = createAutonomousTask({ objective: "edit allowed.ts", targetSnapshot: target, workspaceRoots: roots, permissionSnapshot: permissions, constraints: { strategy: "DIRECT" }, planningContext: { relevantFiles: ["allowed.ts"] } });
    const plan = createTaskPlan("edit allowed.ts", "DIRECT", 1, base.planningContext);
    const placedPlan = { ...plan, nodes: plan.nodes.map((node) => node.nodeId === "implement" ? { ...node, executionPlacement: { kind: "remote_node" as const, targetId: "remote-a", nodeId: node.nodeId, reason: "test placement" } } : node) };
    const runtime = defaultAutonomousTaskRuntime(target, { remote_node: vi.fn(async () => ({ ok: false, failureCode: "EXECUTION_TARGET_LOST" as const, summary: "EXECUTION_TARGET_LOST: remote disappeared" })) });
    const result = await runAutonomousTask({ task: installTaskPlan(base, placedPlan), resolvedTarget: target, runtime });
    expect(result.outcome).toBe("EXECUTION_TARGET_LOST");
    expect(result.repairRounds).toBe(0);
  });
});
