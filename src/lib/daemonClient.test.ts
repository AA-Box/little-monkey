import { describe, expect, it } from "vitest";
import {
  MAX_REMOTE_ARTIFACT_BYTES,
  isDaemonManagedRun,
  type DaemonQueueRequest,
  type RemotePairRequest,
  validateDaemonQueuePolicy,
  validateRemotePairRequest,
} from "./daemonClient";

const base: DaemonQueueRequest = {
  recipe: "review",
  runKey: null,
  priority: 0,
  maxAttempts: 1,
  maxRuntimeSeconds: 3600,
  maxMemoryMb: null,
  ownedWorktree: true,
  repository: "/workspace/repo",
  branchPrefix: "codex/background/",
  allowedRemotes: ["origin"],
  allowCommit: true,
  allowPush: false,
  allowCreatePullRequest: false,
  allowReviewComment: false,
};

describe("daemon queue policy", () => {
  it("identifies only exact daemon-owned run ids for process controls", () => {
    const managed = ["run-daemon-one", "run-daemon-two"];
    expect(isDaemonManagedRun("run-daemon-one", managed)).toBe(true);
    expect(isDaemonManagedRun("run-desktop", managed)).toBe(false);
    expect(isDaemonManagedRun("run-daemon-one/../escape", managed)).toBe(false);
  });

  it("accepts a bounded local owned-worktree job", () => {
    expect(validateDaemonQueuePolicy(base)).toEqual([]);
  });

  it("flags write expansion outside the isolation policy", () => {
    const warnings = validateDaemonQueuePolicy({
      ...base,
      ownedWorktree: false,
      branchPrefix: "main/",
      allowCreatePullRequest: true,
    });
    expect(warnings).toHaveLength(3);
  });
});

const pair: RemotePairRequest = {
  output: "/tmp/little-monkey-pairing.json",
  expiresMinutes: 15,
  actions: ["view-runs", "view-events", "read-artifacts"],
  runIds: ["run-one"],
  workspaceIds: [],
  maxArtifactBytes: 8 * 1024 * 1024,
};

describe("remote pairing policy", () => {
  it("accepts a bounded exact-run invitation", () => {
    expect(validateRemotePairRequest(pair)).toEqual([]);
  });

  it("rejects the old invalid-by-default empty scope and oversized artifact budget", () => {
    const warnings = validateRemotePairRequest({
      ...pair,
      runIds: [],
      maxArtifactBytes: 64 * 1024 * 1024,
    });
    expect(warnings).toContain("Declare at least one exact run ID or workspace ID.");
    expect(warnings).toContain("Artifact access must be limited to between 1 byte and 32 MiB.");
  });

  it("enforces action dependencies, expiry, identifiers, and protocol limits", () => {
    const warnings = validateRemotePairRequest({
      ...pair,
      expiresMinutes: 0,
      actions: ["approve", "approve", "unknown"],
      runIds: ["../escape"],
      workspaceIds: Array.from({ length: 129 }, (_, index) => `workspace-${index}`),
      maxArtifactBytes: MAX_REMOTE_ARTIFACT_BYTES + 1,
    });
    expect(warnings).toHaveLength(7);
    expect(warnings.some((warning) => warning.includes("view-runs"))).toBe(true);
  });
});
