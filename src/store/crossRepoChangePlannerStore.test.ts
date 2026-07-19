import { beforeEach, describe, expect, it, vi } from "vitest";

import type { CrossRepoPlan } from "../lib/crossRepoChangePlanner";
import type { ConfirmationPreview, OwnedWorktreeRecord } from "../lib/gitDelivery";
import type { WorkspaceRootInfo } from "./workspaceStore";

const mocks = vi.hoisted(() => ({
  generateCrossRepoPlan: vi.fn(),
  prepareDeliveryMutation: vi.fn(),
  executeDeliveryMutation: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => false,
}));

vi.mock("../lib/crossRepoChangePlanner", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/crossRepoChangePlanner")>()),
  generateCrossRepoPlan: (...args: unknown[]) => mocks.generateCrossRepoPlan(...args),
}));

vi.mock("../lib/gitDelivery", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/gitDelivery")>()),
  prepareDeliveryMutation: (...args: unknown[]) => mocks.prepareDeliveryMutation(...args),
  executeDeliveryMutation: (...args: unknown[]) => mocks.executeDeliveryMutation(...args),
}));

import { useCrossRepoChangePlannerStore } from "./crossRepoChangePlannerStore";
import { useWorkspaceStore } from "./workspaceStore";

const ROOTS: WorkspaceRootInfo[] = [
  { id: "root-api", path: "/work/api", label: "api", is_primary: true },
  { id: "root-web", path: "/work/web", label: "web", is_primary: false },
];

// The real store (not a module mock — see the note above about avoiding
// brittle partial mocks of a module several other stores import
// transitively): just seed its actual state with fixture roots.
useWorkspaceStore.setState({ roots: ROOTS });

function plan(): CrossRepoPlan {
  return {
    planId: "plan-1234-5678",
    description: "Add a v2 field end to end",
    createdAtMs: 1,
    notes: "Ship the API first.",
    steps: [
      {
        stepId: "step-api",
        rootId: "root-api",
        rootLabel: "api",
        rootPath: "/work/api",
        order: 1,
        summary: "Add the field to the API.",
        changes: "New column + handler.",
        risks: "None.",
        rollback: "Drop the column.",
        dependsOnRootIds: [],
      },
      {
        stepId: "step-web",
        rootId: "root-web",
        rootLabel: "web",
        rootPath: "/work/web",
        order: 2,
        summary: "Wire the client up to the new field.",
        changes: "New form field.",
        risks: "Breaks if API isn't deployed first.",
        rollback: "Hide the form field.",
        dependsOnRootIds: ["root-api"],
      },
    ],
  };
}

function reset() {
  useCrossRepoChangePlannerStore.setState({
    description: "",
    plan: null,
    status: null,
    approvedAtMs: null,
    gitConfigByStep: {},
    createdBranchByStep: {},
    preparingStepId: null,
    pendingMutation: null,
    preview: null,
    busy: {},
    error: null,
    notice: null,
  });
  mocks.generateCrossRepoPlan.mockReset();
  mocks.prepareDeliveryMutation.mockReset();
  mocks.executeDeliveryMutation.mockReset();
}

describe("crossRepoChangePlannerStore", () => {
  beforeEach(reset);

  it("generates a draft plan and seeds default git config per step", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    useCrossRepoChangePlannerStore.getState().setDescription("Add a v2 field end to end");

    await useCrossRepoChangePlannerStore.getState().generate();

    const state = useCrossRepoChangePlannerStore.getState();
    expect(state.plan?.steps).toHaveLength(2);
    expect(state.status).toBe("draft");
    expect(state.gitConfigByStep["step-api"]).toMatchObject({ baseRef: "main", label: "api" });
    expect(state.gitConfigByStep["step-api"].branchPrefix).toContain("cross-repo/");
    expect(mocks.generateCrossRepoPlan).toHaveBeenCalledWith("Add a v2 field end to end", ROOTS);
  });

  it("blocks branch preparation until the plan is explicitly approved", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();

    await expect(
      useCrossRepoChangePlannerStore.getState().prepareBranchForStep("step-api"),
    ).rejects.toThrow(/Approve the plan/);
    expect(mocks.prepareDeliveryMutation).not.toHaveBeenCalled();

    useCrossRepoChangePlannerStore.getState().approvePlan();
    expect(useCrossRepoChangePlannerStore.getState().status).toBe("approved");
  });

  it("edits a step's text fields without touching other steps", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();

    useCrossRepoChangePlannerStore.getState().updateStepField("step-api", "risks", "Actually, watch for null migrations.");

    const steps = useCrossRepoChangePlannerStore.getState().plan!.steps;
    expect(steps.find((s) => s.stepId === "step-api")!.risks).toBe("Actually, watch for null migrations.");
    expect(steps.find((s) => s.stepId === "step-web")!.risks).toBe("Breaks if API isn't deployed first.");
  });

  it("reorders steps and renumbers them contiguously", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();

    useCrossRepoChangePlannerStore.getState().moveStep("step-web", "up");

    const steps = useCrossRepoChangePlannerStore.getState().plan!.steps;
    expect(steps.map((s) => s.stepId)).toEqual(["step-web", "step-api"]);
    expect(steps.map((s) => s.order)).toEqual([1, 2]);
  });

  it("prepares and confirms a branch only after approval, recording the created worktree", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();
    useCrossRepoChangePlannerStore.getState().updateGitConfig("step-api", { repositorySlug: "owner/api" });
    useCrossRepoChangePlannerStore.getState().approvePlan();

    const preview: ConfirmationPreview = {
      digest: "digest-1",
      action: "create_worktree",
      summary: "Create owned worktree",
      impact: "Creates a local branch only.",
      repositorySlug: "owner/api",
      branch: null,
      external: false,
      expiresAtMs: Date.now() + 60_000,
      confirmationPhrase: "CREATE BRANCH",
    };
    mocks.prepareDeliveryMutation.mockResolvedValue(preview);
    const created: OwnedWorktreeRecord = {
      marker: {
        schemaVersion: 1,
        worktreeId: "wt-1",
        leaseNonce: "lease-1",
        repositoryId: "repo-1",
        repositorySlug: "owner/api",
        repositoryRoot: "/work/api",
        commonGitDir: "/work/api/.git",
        canonicalPath: "/app/worktrees/wt-1",
        branch: "cross-repo/plan-123/api",
        baseOid: "a".repeat(40),
        policy: {
          allowedRemotes: ["origin"],
          branchPrefix: "cross-repo/plan-1234-5/",
          protectedBranches: ["main", "master"],
          allowPush: false,
          allowCreatePullRequest: false,
          allowReviewComment: false,
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
    mocks.executeDeliveryMutation.mockResolvedValue(created);

    const returnedPreview = await useCrossRepoChangePlannerStore.getState().prepareBranchForStep("step-api");
    expect(returnedPreview).toEqual(preview);
    expect(mocks.prepareDeliveryMutation).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "create_worktree",
        payload: expect.objectContaining({
          repositoryRoot: "/work/api",
          repositorySlug: "owner/api",
          allowPush: false,
          allowCreatePullRequest: false,
        }),
      }),
    );

    const result = await useCrossRepoChangePlannerStore.getState().confirmBranch("CREATE BRANCH");
    expect(result).toEqual(created);
    const state = useCrossRepoChangePlannerStore.getState();
    expect(state.createdBranchByStep["step-api"]).toEqual({ worktreeId: "wt-1", branch: "cross-repo/plan-123/api" });
    expect(state.preview).toBeNull();
    expect(state.pendingMutation).toBeNull();
  });

  it("rejects an invalid repository slug via the reused gitDelivery validator before calling the backend", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();
    useCrossRepoChangePlannerStore.getState().approvePlan();
    // Left blank — validateCreateRequest requires "owner/repo".

    await expect(
      useCrossRepoChangePlannerStore.getState().prepareBranchForStep("step-api"),
    ).rejects.toThrow(/owner\/name/);
    expect(mocks.prepareDeliveryMutation).not.toHaveBeenCalled();
  });

  it("startOver clears the plan and all derived state", async () => {
    mocks.generateCrossRepoPlan.mockResolvedValue(plan());
    await useCrossRepoChangePlannerStore.getState().generate();
    useCrossRepoChangePlannerStore.getState().approvePlan();

    useCrossRepoChangePlannerStore.getState().startOver();

    const state = useCrossRepoChangePlannerStore.getState();
    expect(state.plan).toBeNull();
    expect(state.status).toBeNull();
    expect(state.gitConfigByStep).toEqual({});
  });
});
