import { describe, expect, it } from "vitest";

import { checkpointChainBlockReason, type CheckpointChainLink } from "./agentLoop";

function link(overrides: Partial<CheckpointChainLink> & { id: string }): CheckpointChainLink {
  return { shellRan: false, prevId: null, ...overrides };
}

describe("checkpointChainBlockReason", () => {
  it("returns null for an unbroken, shell-free chain", () => {
    // Newest-first, each correctly linking to the next-older survivor.
    const checkpoints = [
      link({ id: "c", prevId: "b" }),
      link({ id: "b", prevId: "a" }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
    expect(checkpointChainBlockReason(checkpoints, 1)).toBeNull();
    expect(checkpointChainBlockReason(checkpoints, 2)).toBeNull();
  });

  it("flags a pruned gap when a checkpoint's prevId doesn't match the next surviving entry", () => {
    // B was pruned: C's prevId still points at it, but the next surviving
    // entry is A.
    const checkpoints = [link({ id: "c", prevId: "b" }), link({ id: "a", prevId: null })];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("prunedGap");
    // The gap sits between index 0 and 1, so it must not affect a
    // "Restore to here" targeting only the newest checkpoint itself.
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });

  it("flags a shell run anywhere in the newest-to-target span", () => {
    const checkpoints = [
      link({ id: "c", prevId: "b" }),
      link({ id: "b", prevId: "a", shellRan: true }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("shellRan");
    expect(checkpointChainBlockReason(checkpoints, 2)).toBe("shellRan");
    // The shell run is at index 1, beyond a target of only the newest row.
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });

  it("prefers reporting a pruned gap over a shell run when both are present", () => {
    const checkpoints = [
      link({ id: "c", prevId: "b", shellRan: true }),
      link({ id: "a", prevId: null }),
    ];

    expect(checkpointChainBlockReason(checkpoints, 1)).toBe("prunedGap");
  });

  it("does not flag a session's first checkpoint (null prevId) as a gap", () => {
    const checkpoints = [link({ id: "a", prevId: null })];
    expect(checkpointChainBlockReason(checkpoints, 0)).toBeNull();
  });
});
