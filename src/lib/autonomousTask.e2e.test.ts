import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

import { createAutonomousTask, type AutonomousTask } from "./autonomousTask";
import { runAutonomousTask, type AutonomousTaskRuntime } from "./autonomousTaskRunner";

const target = { kind: "provider", key: "provider:test:model", label: "test" } as never;
const roots = [{ id: "root", path: "/tmp", label: "workspace", is_primary: true }];
const permissions = { mode: "auto", unattended: true, allowNetwork: false, allowExternalMutations: false };
let repository: string;

function evidence(current: AutonomousTask, criterionId: string, passed: boolean) {
  return { evidenceId: `${criterionId}-${Date.now()}`, criterionId, name: "temporary-repository", passed, authoritative: true, stale: false, summary: passed ? "passed" : "failed", exitCode: passed ? 0 : 1, durationMs: 1, createdAtMs: Date.now(), testedRevision: current.workspaceRevision };
}

afterEach(() => { if (repository) rmSync(repository, { recursive: true, force: true }); });

describe("autonomous task temporary-repository execution", () => {
  it("mutates a real git repository and requires current-revision evidence", async () => {
    repository = mkdtempSync(join(tmpdir(), "little-monkey-autonomous-e2e-"));
    writeFileSync(join(repository, "bug.txt"), "bad\n");
    execFileSync("git", ["init", "-q"], { cwd: repository });
    execFileSync("git", ["config", "user.email", "test@example.invalid"], { cwd: repository });
    execFileSync("git", ["config", "user.name", "Test"], { cwd: repository });
    execFileSync("git", ["add", "."], { cwd: repository });
    execFileSync("git", ["commit", "-qm", "baseline"], { cwd: repository });
    const initial = createAutonomousTask({ objective: "fix the bug in bug.txt", targetSnapshot: target, workspaceRoots: roots, permissionSnapshot: permissions, constraints: { strategy: "DIRECT" }, planningContext: { relevantFiles: ["bug.txt"] } });
    const runtime: AutonomousTaskRuntime = {
      executeNode: async (_current, node) => { if (node.taskClass === "implementation") writeFileSync(join(repository, "bug.txt"), "fixed\n"); return { ok: true, summary: node.nodeId, workspaceRevision: "git-working-tree" }; },
      verify: async (current) => { execFileSync("git", ["diff", "--check"], { cwd: repository }); const criterion = current.acceptanceCriteria.find((candidate) => candidate.method === "verification_command")!; return { ok: true, summary: "git diff --check passed", evidence: [evidence(current, criterion.id, true)] }; },
      review: async (current) => ({ ok: true, summary: "review passed", evidence: current.acceptanceCriteria.filter((criterion) => criterion.method === "review").map((criterion) => evidence(current, criterion.id, true)) }),
    };
    const result = await runAutonomousTask({ task: initial, resolvedTarget: { kind: "provider", providerId: "test", model: "model" } as never, runtime });
    expect(result.outcome).toBe("SUCCEEDED");
    expect(execFileSync("git", ["show", "HEAD:bug.txt"], { cwd: repository, encoding: "utf8" })).toBe("bad\n");
    expect(readFileSync(join(repository, "bug.txt"), "utf8")).toBe("fixed\n");
  });
});
