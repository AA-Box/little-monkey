import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  listOwnedWorktrees: vi.fn(),
  inspectOwnedWorktree: vi.fn(),
  prepareDeliveryMutation: vi.fn(),
  executeDeliveryMutation: vi.fn(),
  deliveryAudit: vi.fn(),
  deliveryReconciliations: vi.fn(),
  githubAuthStatus: vi.fn(),
  githubIssue: vi.fn(),
  githubPullRequest: vi.fn(),
  githubReviewThreads: vi.fn(),
  githubChecks: vi.fn(),
  reviewPullRequest: vi.fn(),
  reviewReports: vi.fn(),
}));

vi.mock("../lib/gitDelivery", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/gitDelivery")>(),
  ...api,
}));

import type { ConfirmationPreview, OwnedWorktreeRecord, WorktreeInspection } from "../lib/gitDelivery";
import { useGitDeliveryStore } from "./gitDeliveryStore";

const record: OwnedWorktreeRecord = {
  marker: {
    schemaVersion: 1,
    worktreeId: "wt-fixture",
    leaseNonce: "lease-fixture",
    repositoryId: "repo-fixture",
    repositorySlug: "owner/repo",
    repositoryRoot: "/workspace/repo",
    commonGitDir: "/workspace/repo/.git",
    canonicalPath: "/app/worktrees/wt-fixture",
    branch: "codex/delivery/issue-7",
    baseOid: "a".repeat(40),
    policy: {
      allowedRemotes: ["origin"],
      branchPrefix: "codex/delivery/",
      protectedBranches: ["main"],
      allowPush: true,
      allowCreatePullRequest: true,
      allowReviewComment: true,
      allowForkWrites: false,
    },
    createdAtMs: 1,
  },
  state: "active",
  locked: false,
  lockReason: null,
  archivePath: null,
  createdAtMs: 1,
  updatedAtMs: 1,
};

const inspection: WorktreeInspection = {
  worktree: record,
  headOid: "b".repeat(40),
  ahead: 1,
  behind: 0,
  dirty: true,
  cleanupBlocked: true,
  files: [{ path: "src/lib.rs", oldPath: null, indexStatus: " ", worktreeStatus: "M", untracked: false, ignored: false }],
  diffs: {
    staged: { text: "", truncated: false },
    unstaged: { text: "diff", truncated: false },
    head: { text: "diff", truncated: false },
  },
};

const mutation = {
  kind: "stage" as const,
  payload: { worktreeId: "wt-fixture", paths: ["src/lib.rs"] },
};

const preview: ConfirmationPreview = {
  digest: "c".repeat(64),
  action: "stage",
  summary: "Stage selected paths",
  impact: "Index only",
  repositorySlug: "owner/repo",
  branch: record.marker.branch,
  external: false,
  expiresAtMs: Date.now() + 60_000,
  confirmationPhrase: `CONFIRM ${"c".repeat(12)}`,
};

beforeEach(() => {
  for (const mock of Object.values(api)) mock.mockReset();
  api.listOwnedWorktrees.mockResolvedValue([record]);
  api.inspectOwnedWorktree.mockResolvedValue(inspection);
  api.deliveryAudit.mockResolvedValue([]);
  api.deliveryReconciliations.mockResolvedValue([]);
  useGitDeliveryStore.setState({
    worktrees: [], selectedWorktreeId: null, inspection: null, auth: null,
    issue: null, pullRequest: null, reviewThreads: null, checks: null,
    reports: [], audit: [], reconciliations: [], pendingMutation: null, preview: null,
    busy: {}, error: null, notice: null,
  });
});

describe("gitDeliveryStore", () => {
  it("selects and inspects the first active owned worktree", async () => {
    await useGitDeliveryStore.getState().refresh();
    expect(useGitDeliveryStore.getState().selectedWorktreeId).toBe("wt-fixture");
    expect(useGitDeliveryStore.getState().inspection).toEqual(inspection);
    expect(api.inspectOwnedWorktree).toHaveBeenCalledWith("wt-fixture");
  });

  it("executes only the exact prepared digest and refreshes inspection plus audit", async () => {
    api.prepareDeliveryMutation.mockResolvedValue(preview);
    api.executeDeliveryMutation.mockResolvedValue({ staged: true });
    await useGitDeliveryStore.getState().refresh();
    await useGitDeliveryStore.getState().prepare(mutation);
    await useGitDeliveryStore.getState().executePrepared(preview.confirmationPhrase);
    expect(api.executeDeliveryMutation).toHaveBeenCalledWith(
      mutation,
      preview.digest,
      preview.confirmationPhrase,
    );
    expect(useGitDeliveryStore.getState().preview).toBeNull();
    expect(api.deliveryAudit).toHaveBeenCalledWith(100);
    expect(api.deliveryReconciliations).toHaveBeenCalledOnce();
  });

  it("loads unresolved executions with the audit and resolves only through a new preview", async () => {
    const execution = {
      requestDigest: "d".repeat(64),
      action: "push",
      target: "owner/repo:codex/delivery/issue-7",
      external: true,
      state: "needs_reconciliation",
      executorInstance: "previous-process",
      confirmedAtMs: 1,
      startedAtMs: 1,
      finishedAtMs: 2,
      result: null,
      error: "connection ended after dispatch",
      resolution: null,
      resolutionNote: null,
      updatedAtMs: 2,
    };
    api.deliveryReconciliations.mockResolvedValue([execution]);
    await useGitDeliveryStore.getState().refreshAudit();
    expect(useGitDeliveryStore.getState().reconciliations).toEqual([execution]);

    const resolution = {
      kind: "resolve_reconciliation" as const,
      payload: {
        requestDigest: execution.requestDigest,
        resolution: "completed" as const,
        note: "Verified the exact branch OID on GitHub.",
      },
    };
    api.prepareDeliveryMutation.mockResolvedValue({
      ...preview,
      action: "resolve_reconciliation",
      summary: "Resolve reconciliation",
    });
    await useGitDeliveryStore.getState().prepare(resolution);
    expect(api.prepareDeliveryMutation).toHaveBeenCalledWith(resolution);
    expect(api.executeDeliveryMutation).not.toHaveBeenCalled();
  });

  it("refuses an expired prepared mutation without invoking the backend", async () => {
    useGitDeliveryStore.setState({
      pendingMutation: mutation,
      preview: { ...preview, expiresAtMs: Date.now() - 1 },
    });
    await expect(useGitDeliveryStore.getState().executePrepared(preview.confirmationPhrase))
      .rejects.toThrow("expired");
    expect(api.executeDeliveryMutation).not.toHaveBeenCalled();
  });

  it("keeps a successful issue read when the same number is not a PR", async () => {
    useGitDeliveryStore.setState({ selectedWorktreeId: "wt-fixture" });
    api.githubIssue.mockResolvedValue({ number: 7, title: "Fixture" });
    api.githubPullRequest.mockRejectedValue(new Error("not a pull request"));
    api.githubReviewThreads.mockRejectedValue(new Error("not a pull request"));
    api.githubChecks.mockRejectedValue(new Error("not a pull request"));
    await useGitDeliveryStore.getState().loadGitHub(7);
    expect(useGitDeliveryStore.getState().issue).toEqual({ number: 7, title: "Fixture" });
    expect(useGitDeliveryStore.getState().pullRequest).toBeNull();
  });
});
