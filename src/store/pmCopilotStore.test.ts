import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  generatePmPlan: vi.fn(),
  savePmPlanToWorkspace: vi.fn(),
  cancelPmPlanGeneration: vi.fn(),
}));

vi.mock("../lib/pmCopilot", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/pmCopilot")>()),
  ...api,
}));

import type { ModelTargetSnapshot } from "../lib/modelTargets";
import type { PmPlan } from "../lib/pmCopilot";
import { usePmCopilotStore } from "./pmCopilotStore";

const TARGET: ModelTargetSnapshot = {
  kind: "provider",
  key: "provider:test:model",
  label: "Test Provider",
  displayName: "test-model",
  providerId: "test",
  endpoint: "https://provider.test/v1",
  model: "test-model",
  credentialRefId: "keychain:com.littlemonkey.app:test",
  capabilities: {
    toolCalling: { state: "unknown", evidence: "test" },
    vision: { state: "unknown", evidence: "test" },
  },
  availability: { status: "available", evidence: "test" },
};

const PLAN: PmPlan = {
  goal: "Export data as CSV",
  prdSummary: "Let users export their data.",
  userStories: [{ asA: "user", iWant: "to export data", soThat: "I can analyze it" }],
  acceptanceCriteria: ["Export produces a valid file"],
  risks: [{ description: "Large exports time out", severity: "high", mitigation: "Stream it" }],
  milestones: [{ name: "MVP", summary: "Basic export" }],
};

function resetStore(): void {
  usePmCopilotStore.setState({
    draftId: usePmCopilotStore.getState().draftId,
    goal: "",
    status: "idle",
    error: null,
    plan: null,
    modelLabel: null,
    generatedAtMs: null,
    slug: "",
    slugTouched: false,
    saveStatus: "idle",
    saveError: null,
    savedPath: null,
  });
}

beforeEach(() => {
  api.generatePmPlan.mockReset();
  api.savePmPlanToWorkspace.mockReset();
  api.cancelPmPlanGeneration.mockReset();
  resetStore();
});

describe("pmCopilotStore", () => {
  it("derives a filesystem-safe slug from the goal until the user edits it directly", () => {
    usePmCopilotStore.getState().setGoal("Export Data as CSV!");
    expect(usePmCopilotStore.getState().slug).toBe("export-data-as-csv");

    usePmCopilotStore.getState().setSlug("my-custom-name");
    expect(usePmCopilotStore.getState().slugTouched).toBe(true);

    // Further goal edits no longer clobber the user's explicit filename choice.
    usePmCopilotStore.getState().setGoal("A completely different goal");
    expect(usePmCopilotStore.getState().slug).toBe("my-custom-name");
  });

  it("generates a plan and records the model label and status", async () => {
    api.generatePmPlan.mockResolvedValue({ plan: PLAN, target: TARGET });
    usePmCopilotStore.getState().setGoal("Export data as CSV");

    await usePmCopilotStore.getState().generate();

    const state = usePmCopilotStore.getState();
    expect(state.status).toBe("ready");
    expect(state.plan).toEqual(PLAN);
    expect(state.modelLabel).toBe("Test Provider · test-model");
    expect(state.generatedAtMs).not.toBeNull();
    expect(api.generatePmPlan).toHaveBeenCalledWith(state.draftId, "Export data as CSV");
  });

  it("surfaces a generation error without touching any previous plan", async () => {
    api.generatePmPlan.mockRejectedValue(new Error("model unavailable"));
    usePmCopilotStore.getState().setGoal("Export data as CSV");

    await usePmCopilotStore.getState().generate();

    const state = usePmCopilotStore.getState();
    expect(state.status).toBe("error");
    expect(state.error).toBe("model unavailable");
    expect(state.plan).toBeNull();
  });

  it("edits every section of a generated plan without losing untouched fields", async () => {
    api.generatePmPlan.mockResolvedValue({ plan: PLAN, target: TARGET });
    usePmCopilotStore.getState().setGoal("Export data as CSV");
    await usePmCopilotStore.getState().generate();

    const store = usePmCopilotStore.getState();
    store.updatePrdSummary("An edited summary.");
    store.addUserStory();
    store.updateUserStory(1, "asA", "power user");
    store.addAcceptanceCriterion();
    store.updateAcceptanceCriterion(1, "Export completes within 30 seconds");
    store.addRisk();
    store.updateRisk(1, "description", "New risk");
    store.updateRisk(1, "severity", "low");
    store.addMilestone();
    store.updateMilestone(1, "name", "GA");
    store.removeAcceptanceCriterion(0);

    const plan = usePmCopilotStore.getState().plan!;
    expect(plan.prdSummary).toBe("An edited summary.");
    expect(plan.userStories).toHaveLength(2);
    expect(plan.userStories[1].asA).toBe("power user");
    expect(plan.acceptanceCriteria).toEqual(["Export completes within 30 seconds"]);
    expect(plan.risks[1]).toMatchObject({ description: "New risk", severity: "low" });
    expect(plan.milestones[1].name).toBe("GA");
  });

  it("removes rows from every editable section", async () => {
    api.generatePmPlan.mockResolvedValue({ plan: PLAN, target: TARGET });
    usePmCopilotStore.getState().setGoal("Export data as CSV");
    await usePmCopilotStore.getState().generate();

    const store = usePmCopilotStore.getState();
    store.removeUserStory(0);
    store.removeRisk(0);
    store.removeMilestone(0);

    const plan = usePmCopilotStore.getState().plan!;
    expect(plan.userStories).toHaveLength(0);
    expect(plan.risks).toHaveLength(0);
    expect(plan.milestones).toHaveLength(0);
  });

  it("saves the current plan to the workspace and records the returned path", async () => {
    api.generatePmPlan.mockResolvedValue({ plan: PLAN, target: TARGET });
    api.savePmPlanToWorkspace.mockResolvedValue("docs/product/export-data-as-csv.md");
    usePmCopilotStore.getState().setGoal("Export data as CSV");
    await usePmCopilotStore.getState().generate();

    await usePmCopilotStore.getState().save();

    const state = usePmCopilotStore.getState();
    expect(state.saveStatus).toBe("saved");
    expect(state.savedPath).toBe("docs/product/export-data-as-csv.md");
    expect(api.savePmPlanToWorkspace).toHaveBeenCalledWith(expect.stringContaining("# Export data as CSV"), "export-data-as-csv");
  });

  it("surfaces a save error without discarding the plan", async () => {
    api.generatePmPlan.mockResolvedValue({ plan: PLAN, target: TARGET });
    api.savePmPlanToWorkspace.mockRejectedValue(new Error("Open a workspace folder before saving."));
    usePmCopilotStore.getState().setGoal("Export data as CSV");
    await usePmCopilotStore.getState().generate();

    await usePmCopilotStore.getState().save();

    const state = usePmCopilotStore.getState();
    expect(state.saveStatus).toBe("error");
    expect(state.saveError).toBe("Open a workspace folder before saving.");
    expect(state.plan).not.toBeNull();
  });

  it("refuses to save before a plan has been generated", async () => {
    await usePmCopilotStore.getState().save();
    const state = usePmCopilotStore.getState();
    expect(state.saveStatus).toBe("error");
    expect(api.savePmPlanToWorkspace).not.toHaveBeenCalled();
  });

  it("reset() clears the draft and mints a new draft id", () => {
    const originalDraftId = usePmCopilotStore.getState().draftId;
    usePmCopilotStore.getState().setGoal("Something");
    usePmCopilotStore.getState().reset();

    const state = usePmCopilotStore.getState();
    expect(state.goal).toBe("");
    expect(state.plan).toBeNull();
    expect(state.status).toBe("idle");
    expect(state.draftId).not.toBe(originalDraftId);
  });
});
