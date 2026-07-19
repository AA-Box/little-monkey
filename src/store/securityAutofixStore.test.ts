import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  runSecurityScan: vi.fn(),
  defaultProposeFixCallModel: vi.fn(),
  proposeFixForFinding: vi.fn(),
  createIsolatedBranchForFinding: vi.fn(),
  runSecurityAutofixAgent: vi.fn(),
}));

vi.mock("../lib/securityAutofix", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/securityAutofix")>()),
  ...api,
}));

import type { SecurityFinding, SecurityFixProposal } from "../lib/securityAutofix";
import { __resetSecurityAutofixControllersForTests, useSecurityAutofixStore } from "./securityAutofixStore";

function fixtureFinding(overrides: Partial<SecurityFinding> = {}): SecurityFinding {
  return {
    id: "dep-1",
    kind: "dependency",
    severity: "high",
    title: "lodash: Prototype Pollution",
    description: "desc",
    detectedAtMs: 1,
    dependency: {
      packageName: "lodash",
      currentVersion: "4.17.15",
      patchedVersions: ">=4.17.19",
      vulnerableRange: "<4.17.19",
      advisoryTitle: "Prototype Pollution",
      advisoryUrl: null,
      advisoryId: "1",
    },
    ...overrides,
  };
}

function fixtureProposal(overrides: Partial<SecurityFixProposal> = {}): SecurityFixProposal {
  return {
    findingId: "dep-1",
    exploitabilityNote: "note",
    proposedFix: "fix",
    testPlan: "plan",
    generatedAtMs: 1,
    source: "model",
    ...overrides,
  };
}

beforeEach(() => {
  for (const mock of Object.values(api)) mock.mockReset();
  __resetSecurityAutofixControllersForTests();
  useSecurityAutofixStore.setState({
    findings: [],
    proposals: {},
    proposing: {},
    applyState: {},
    repositorySlug: "",
    scanning: false,
    scanError: null,
    error: null,
  });
});

describe("securityAutofixStore", () => {
  it("scan populates findings and surfaces the audit error separately", async () => {
    const finding = fixtureFinding();
    api.runSecurityScan.mockResolvedValue({ findings: [finding], auditError: "pnpm not found" });

    await useSecurityAutofixStore.getState().scan();

    expect(useSecurityAutofixStore.getState().findings).toEqual([finding]);
    expect(useSecurityAutofixStore.getState().scanError).toBe("pnpm not found");
    expect(useSecurityAutofixStore.getState().scanning).toBe(false);
  });

  it("scan surfaces a thrown error onto the store without leaving `scanning` stuck true", async () => {
    api.runSecurityScan.mockRejectedValue(new Error("boom"));
    await expect(useSecurityAutofixStore.getState().scan()).rejects.toThrow("boom");
    expect(useSecurityAutofixStore.getState().error).toBe("boom");
    expect(useSecurityAutofixStore.getState().scanning).toBe(false);
  });

  it("proposeFix stores the resulting proposal keyed by finding id", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({ findings: [finding] });
    const proposal = fixtureProposal();
    const callModel = vi.fn();
    api.defaultProposeFixCallModel.mockResolvedValue(callModel);
    api.proposeFixForFinding.mockResolvedValue(proposal);

    await useSecurityAutofixStore.getState().proposeFix(finding.id);

    expect(useSecurityAutofixStore.getState().proposals[finding.id]).toEqual(proposal);
    expect(useSecurityAutofixStore.getState().proposing[finding.id]).toBe(false);
    expect(api.proposeFixForFinding).toHaveBeenCalledWith(finding, callModel);
  });

  it("proposeFix rejects for an unknown finding id without calling the model", async () => {
    await expect(useSecurityAutofixStore.getState().proposeFix("missing")).rejects.toThrow('Unknown finding "missing"');
    expect(api.defaultProposeFixCallModel).not.toHaveBeenCalled();
  });

  it("applyFix requires a proposal to already exist for the finding", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({ findings: [finding], repositorySlug: "owner/repo" });
    await expect(useSecurityAutofixStore.getState().applyFix(finding.id)).rejects.toThrow(
      "Propose a fix for this finding first.",
    );
    expect(api.createIsolatedBranchForFinding).not.toHaveBeenCalled();
  });

  it("applyFix requires a repository slug before creating a branch", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({
      findings: [finding],
      proposals: { [finding.id]: fixtureProposal() },
      repositorySlug: "",
    });
    await expect(useSecurityAutofixStore.getState().applyFix(finding.id)).rejects.toThrow(
      "Enter the GitHub repository (owner/repository) first.",
    );
  });

  it("applyFix drives branch creation then the headless agent turn through to done", async () => {
    const finding = fixtureFinding();
    const proposal = fixtureProposal();
    useSecurityAutofixStore.setState({
      findings: [finding],
      proposals: { [finding.id]: proposal },
      repositorySlug: "owner/repo",
    });
    api.createIsolatedBranchForFinding.mockResolvedValue({
      worktreeId: "wt-1",
      branch: "security-autofix/security-dependency-lodash",
      workspaceLabel: "security-autofix-lodash",
      canonicalPath: "/repo-worktrees/x",
    });
    api.runSecurityAutofixAgent.mockResolvedValue({
      outcome: "completed",
      summary: "Upgraded lodash.",
      durableRunId: "run-abc",
    });

    await useSecurityAutofixStore.getState().applyFix(finding.id);

    const state = useSecurityAutofixStore.getState().applyState[finding.id];
    expect(state.status).toBe("done");
    expect(state.summary).toBe("Upgraded lodash.");
    expect(state.durableRunId).toBe("run-abc");
    expect(state.branch).toBe("security-autofix/security-dependency-lodash");
    expect(api.runSecurityAutofixAgent).toHaveBeenCalledWith(
      expect.objectContaining({ finding, proposal, branch: "security-autofix/security-dependency-lodash" }),
    );
  });

  it("applyFix records a failed branch creation as an error state without invoking the agent", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({
      findings: [finding],
      proposals: { [finding.id]: fixtureProposal() },
      repositorySlug: "owner/repo",
    });
    api.createIsolatedBranchForFinding.mockRejectedValue(new Error("worktree creation failed"));

    await expect(useSecurityAutofixStore.getState().applyFix(finding.id)).rejects.toThrow("worktree creation failed");

    expect(useSecurityAutofixStore.getState().applyState[finding.id].status).toBe("error");
    expect(useSecurityAutofixStore.getState().applyState[finding.id].error).toBe("worktree creation failed");
    expect(api.runSecurityAutofixAgent).not.toHaveBeenCalled();
  });

  it("applyFix records a cancelled outcome without throwing", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({
      findings: [finding],
      proposals: { [finding.id]: fixtureProposal() },
      repositorySlug: "owner/repo",
    });
    api.createIsolatedBranchForFinding.mockResolvedValue({
      worktreeId: "wt-1",
      branch: "b",
      workspaceLabel: "l",
      canonicalPath: "/x",
    });
    api.runSecurityAutofixAgent.mockResolvedValue({ outcome: "cancelled", summary: "Cancelled by the user.", durableRunId: null });

    await useSecurityAutofixStore.getState().applyFix(finding.id);

    expect(useSecurityAutofixStore.getState().applyState[finding.id].status).toBe("cancelled");
  });

  it("cancelApply aborts the in-flight run's signal", async () => {
    const finding = fixtureFinding();
    useSecurityAutofixStore.setState({
      findings: [finding],
      proposals: { [finding.id]: fixtureProposal() },
      repositorySlug: "owner/repo",
    });
    api.createIsolatedBranchForFinding.mockResolvedValue({
      worktreeId: "wt-1",
      branch: "b",
      workspaceLabel: "l",
      canonicalPath: "/x",
    });
    let capturedSignal: AbortSignal | undefined;
    api.runSecurityAutofixAgent.mockImplementation(async (params: { signal: AbortSignal }) => {
      capturedSignal = params.signal;
      return new Promise(() => {});
    });

    const applyPromise = useSecurityAutofixStore.getState().applyFix(finding.id);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    useSecurityAutofixStore.getState().cancelApply(finding.id);
    expect(capturedSignal?.aborted).toBe(true);

    void applyPromise;
  });

  it("clearError resets the error field", () => {
    useSecurityAutofixStore.setState({ error: "something broke" });
    useSecurityAutofixStore.getState().clearError();
    expect(useSecurityAutofixStore.getState().error).toBeNull();
  });

  it("setRepositorySlug stores the trimmed-on-use value verbatim", () => {
    useSecurityAutofixStore.getState().setRepositorySlug("owner/repo");
    expect(useSecurityAutofixStore.getState().repositorySlug).toBe("owner/repo");
  });
});
