import { beforeEach, describe, expect, it } from "vitest";

import { selectDeepResearchRuns, useDeepResearchStore } from "./deepResearchStore";
import type { ResearchPlan, ResearchReport, StepOutcome } from "../lib/deepResearch";

const plan: ResearchPlan = {
  question: "question",
  steps: [{ id: "P1", kind: "web", query: "q", rationale: "r" }],
};

const outcome: StepOutcome = {
  step: plan.steps[0],
  status: "searched",
  reason: null,
  evidence: [{ id: "S1", stepId: "P1", kind: "web", sourceLabel: "a", sourceRef: "http://a", snippet: "A" }],
};

const report: ResearchReport = {
  summary: "summary",
  claims: [{ id: "C1", text: "claim", evidenceIds: ["S1"] }],
  openQuestions: [],
  droppedClaimCount: 0,
};

beforeEach(() => {
  useDeepResearchStore.setState({ runs: {}, order: [], selectedRunId: null });
});

describe("deepResearchStore", () => {
  it("creates a run in 'planning' status, newest first, and selects it", () => {
    const first = useDeepResearchStore.getState().create("first question");
    const second = useDeepResearchStore.getState().create("second question");

    const state = useDeepResearchStore.getState();
    expect(state.order).toEqual([second.id, first.id]);
    expect(state.selectedRunId).toBe(second.id);
    expect(state.runs[first.id].status).toBe("planning");
    expect(state.runs[first.id].plan).toBeNull();
  });

  it("setPlan, appendStepResult, and setReport progress a run through to done", () => {
    const run = useDeepResearchStore.getState().create("question");
    useDeepResearchStore.getState().setStatus(run.id, "researching");
    useDeepResearchStore.getState().setPlan(run.id, plan);
    useDeepResearchStore.getState().setPendingStep(run.id, "P1");
    expect(useDeepResearchStore.getState().runs[run.id].pendingStepId).toBe("P1");

    useDeepResearchStore.getState().appendStepResult(run.id, outcome);
    const afterStep = useDeepResearchStore.getState().runs[run.id];
    expect(afterStep.stepResults).toEqual([outcome]);
    // Appending the pending step's own outcome clears pendingStepId.
    expect(afterStep.pendingStepId).toBeNull();

    useDeepResearchStore.getState().setReport(run.id, report);
    const finished = useDeepResearchStore.getState().runs[run.id];
    expect(finished.report).toEqual(report);
    expect(finished.status).toBe("done");
    expect(finished.pendingStepId).toBeNull();
  });

  it("setError marks a run terminal with the given message", () => {
    const run = useDeepResearchStore.getState().create("question");
    useDeepResearchStore.getState().setError(run.id, "boom");
    const failed = useDeepResearchStore.getState().runs[run.id];
    expect(failed.status).toBe("error");
    expect(failed.error).toBe("boom");
  });

  it("remove deletes a run and clears selection if it was selected", () => {
    const run = useDeepResearchStore.getState().create("question");
    useDeepResearchStore.getState().remove(run.id);
    const state = useDeepResearchStore.getState();
    expect(state.runs[run.id]).toBeUndefined();
    expect(state.order).not.toContain(run.id);
    expect(state.selectedRunId).toBeNull();
  });

  it("selectDeepResearchRuns returns runs newest-first", () => {
    const first = useDeepResearchStore.getState().create("first");
    const second = useDeepResearchStore.getState().create("second");
    const runs = selectDeepResearchRuns(useDeepResearchStore.getState());
    expect(runs.map((r) => r.id)).toEqual([second.id, first.id]);
  });
});
