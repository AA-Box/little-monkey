import { beforeEach, describe, expect, it } from "vitest";

import { useSavedWorkflowStore } from "./savedWorkflowStore";
import type { WorkflowSpec } from "../lib/workflow";

function spec(name = "roadmap-audit"): WorkflowSpec {
  return {
    name,
    description: "d",
    phases: [{ title: "P", agents: [{ description: "a", prompt: "p", profile: "explore" }] }],
  };
}

describe("savedWorkflowStore", () => {
  beforeEach(() => {
    useSavedWorkflowStore.setState({ workflows: {} });
  });

  it("upsert keeps the original savedAt across updates and only stamps lastRunAt when ranAt is given", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    const first = useSavedWorkflowStore.getState().workflows["roadmap-audit"];
    expect(first.lastRunAt).toBeUndefined();

    useSavedWorkflowStore.getState().upsert(spec(), 1234);
    const second = useSavedWorkflowStore.getState().workflows["roadmap-audit"];
    expect(second.savedAt).toBe(first.savedAt);
    expect(second.lastRunAt).toBe(1234);

    // A later explicit save (no ranAt) must not erase the run timestamp.
    useSavedWorkflowStore.getState().upsert(spec());
    expect(useSavedWorkflowStore.getState().workflows["roadmap-audit"].lastRunAt).toBe(1234);
  });

  it("remove deletes only the named entry", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    useSavedWorkflowStore.getState().upsert(spec("release-check"));

    useSavedWorkflowStore.getState().remove("roadmap-audit");

    expect(useSavedWorkflowStore.getState().workflows["roadmap-audit"]).toBeUndefined();
    expect(useSavedWorkflowStore.getState().workflows["release-check"]).toBeDefined();
  });
});
