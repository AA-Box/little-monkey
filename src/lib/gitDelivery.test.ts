import { describe, expect, it } from "vitest";

import {
  isExternalMutation,
  type DeliveryMutation,
  type WorktreeCreateRequest,
  validateCreateRequest,
} from "./gitDelivery";

const request: WorktreeCreateRequest = {
  repositoryRoot: "/workspace/repo",
  repositorySlug: "owner/repo",
  baseRef: "main",
  label: "issue-7",
  allowedRemotes: ["origin"],
  branchPrefix: "codex/delivery/",
  protectedBranches: ["main"],
  allowPush: true,
  allowCreatePullRequest: true,
  allowReviewComment: true,
  allowForkWrites: false,
};

describe("git delivery client contract", () => {
  it("accepts an exact owned-branch policy and rejects write expansion", () => {
    expect(validateCreateRequest(request)).toEqual([]);
    expect(validateCreateRequest({
      ...request,
      repositorySlug: "https://github.com/owner/repo",
      branchPrefix: "main",
      allowedRemotes: [],
      allowPush: false,
    })).toHaveLength(4);
  });

  it("serializes closed mutations with camelCase payloads", () => {
    const mutation: DeliveryMutation = {
      kind: "create_draft_pr",
      payload: {
        worktreeId: "wt-fixture",
        base: "main",
        title: "Fixture",
        body: "Body",
      },
    };
    expect(JSON.parse(JSON.stringify(mutation))).toEqual(mutation);
    expect(isExternalMutation(mutation)).toBe(true);
    expect(isExternalMutation({
      kind: "commit",
      payload: { worktreeId: "wt-fixture", paths: ["src/lib.rs"], message: "fix" },
    })).toBe(false);
    expect(JSON.stringify(mutation)).not.toMatch(/force|merge/i);

    const resolution: DeliveryMutation = {
      kind: "resolve_reconciliation",
      payload: {
        requestDigest: "d".repeat(64),
        resolution: "not_applied",
        note: "Verified the remote branch still points to the previous OID.",
      },
    };
    expect(isExternalMutation(resolution)).toBe(false);
    expect(JSON.parse(JSON.stringify(resolution))).toEqual(resolution);
  });
});
