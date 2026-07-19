import { beforeEach, describe, expect, it } from "vitest";

import { buildStarterSuites, emptyResult, type BenchmarkSuite, type LabResult, type ModelSet } from "../lib/compareLab";
import type { ProviderModelTargetSnapshot } from "../lib/modelTargets";
import { COMPARE_LAB_STORAGE_KEY, MAX_RUN_HISTORY, useCompareLabStore } from "./compareLabStore";

function target(id: string, overrides: Partial<ProviderModelTargetSnapshot> = {}): ProviderModelTargetSnapshot {
  return {
    kind: "provider",
    key: `provider:test:${id}`,
    label: "Test Provider",
    displayName: id,
    providerId: "test",
    endpoint: "https://provider.test/v1",
    model: id,
    credentialRefId: "keychain:com.littlemonkey.app:test",
    capabilities: {
      toolCalling: { state: "unknown", evidence: "test" },
      vision: { state: "unknown", evidence: "test" },
    },
    availability: { status: "available", evidence: "test" },
    ...overrides,
  };
}

function emptySuite(overrides: Partial<BenchmarkSuite> = {}): BenchmarkSuite {
  const now = Date.now();
  return { id: "", name: "Suite", description: "", category: "custom", prompts: [], builtIn: false, createdAt: now, updatedAt: now, ...overrides };
}

function emptyModelSet(overrides: Partial<ModelSet> = {}): ModelSet {
  const now = Date.now();
  return { id: "", name: "Set", targets: [], createdAt: now, updatedAt: now, ...overrides };
}

// The store hydrates once at module load from `localStorage` (wrapped in a
// try/catch, so it degrades to the starter defaults under vitest's "node"
// environment where `localStorage` is not defined). Every test resets the
// in-memory state directly via `setState` rather than re-importing the
// module, mirroring `sideTaskStore.test.ts`'s reset pattern.
function reset(): void {
  useCompareLabStore.setState({
    suites: buildStarterSuites(),
    modelSets: [],
    costRates: {},
    runs: [],
    activeRunId: null,
  });
}

beforeEach(reset);

describe("compareLabStore / hydration", () => {
  it("starts with the five starter suites and no model sets, runs, or cost rates", () => {
    const state = useCompareLabStore.getState();
    expect(state.suites).toHaveLength(5);
    expect(state.suites.every((s) => s.builtIn)).toBe(true);
    expect(state.modelSets).toEqual([]);
    expect(state.runs).toEqual([]);
    expect(state.costRates).toEqual({});
  });

  it("exposes the storage key and run-history cap other modules rely on", () => {
    expect(COMPARE_LAB_STORAGE_KEY).toBe("little-monkey-compare-lab");
    expect(MAX_RUN_HISTORY).toBeGreaterThan(0);
  });
});

describe("compareLabStore / suites", () => {
  it("saveSuite assigns a fresh id for a new suite and normalizes its prompts", () => {
    const suite = emptySuite({
      name: "My suite",
      prompts: [{ id: "", text: "Hello", toolsEnabled: true, verifier: null, rubricCriteria: ["Clarity"] }],
    });
    const id = useCompareLabStore.getState().saveSuite(suite);
    expect(id).not.toBe("");
    const saved = useCompareLabStore.getState().suites.find((s) => s.id === id);
    expect(saved).toBeDefined();
    expect(saved?.name).toBe("My suite");
    expect(saved?.prompts[0].id).not.toBe(""); // normalizePrompt fills a real id
    expect(saved?.prompts[0].text).toBe("Hello");
  });

  it("saveSuite with an existing id updates in place instead of duplicating", () => {
    const id = useCompareLabStore.getState().saveSuite(emptySuite({ name: "First" }));
    const countBefore = useCompareLabStore.getState().suites.length;
    useCompareLabStore.getState().saveSuite(emptySuite({ id, name: "Renamed" }));
    const state = useCompareLabStore.getState();
    expect(state.suites).toHaveLength(countBefore);
    expect(state.suites.find((s) => s.id === id)?.name).toBe("Renamed");
  });

  it("removeSuite drops the suite by id", () => {
    const id = useCompareLabStore.getState().saveSuite(emptySuite({ name: "Temp" }));
    useCompareLabStore.getState().removeSuite(id);
    expect(useCompareLabStore.getState().suites.find((s) => s.id === id)).toBeUndefined();
  });

  it("duplicateSuite clones a builtIn starter suite as an editable, deletable copy with fresh prompt ids", () => {
    const source = useCompareLabStore.getState().suites[0];
    const cloneId = useCompareLabStore.getState().duplicateSuite(source.id);
    expect(cloneId).not.toBeNull();
    const clone = useCompareLabStore.getState().suites.find((s) => s.id === cloneId);
    expect(clone).toBeDefined();
    expect(clone?.builtIn).toBe(false);
    expect(clone?.name).toBe(`${source.name} (copy)`);
    expect(clone?.prompts.map((p) => p.id)).not.toEqual(source.prompts.map((p) => p.id));
    expect(clone?.prompts.map((p) => p.text)).toEqual(source.prompts.map((p) => p.text));
  });

  it("duplicateSuite returns null for an unknown id", () => {
    expect(useCompareLabStore.getState().duplicateSuite("missing")).toBeNull();
  });
});

describe("compareLabStore / model sets and cost rates", () => {
  it("saveModelSet creates a new set with a generated id when none is passed", () => {
    const t1 = target("a");
    const id = useCompareLabStore.getState().saveModelSet("My set", [t1]);
    expect(id).not.toBe("");
    const saved = useCompareLabStore.getState().modelSets.find((s) => s.id === id);
    expect(saved?.name).toBe("My set");
    expect(saved?.targets).toEqual([t1]);
  });

  it("saveModelSet with an explicit id updates the existing set in place", () => {
    const id = useCompareLabStore.getState().saveModelSet("Original", [target("a")]);
    useCompareLabStore.getState().saveModelSet("Updated", [target("a"), target("b")], id);
    const state = useCompareLabStore.getState();
    expect(state.modelSets).toHaveLength(1);
    expect(state.modelSets[0].name).toBe("Updated");
    expect(state.modelSets[0].targets).toHaveLength(2);
  });

  it("saveModelSet falls back to 'Untitled set' for a blank name", () => {
    const id = useCompareLabStore.getState().saveModelSet("   ", [target("a")]);
    expect(useCompareLabStore.getState().modelSets.find((s) => s.id === id)?.name).toBe("Untitled set");
  });

  it("removeModelSet drops the set by id", () => {
    const id = useCompareLabStore.getState().saveModelSet("Temp", [target("a")]);
    useCompareLabStore.getState().removeModelSet(id);
    expect(useCompareLabStore.getState().modelSets.find((s) => s.id === id)).toBeUndefined();
  });

  it("setCostRate stores a rate keyed by target key, and null clears it", () => {
    const key = target("a").key;
    useCompareLabStore.getState().setCostRate(key, { inputPerMillionUsd: 1, outputPerMillionUsd: 2 });
    expect(useCompareLabStore.getState().costRates[key]).toEqual({ inputPerMillionUsd: 1, outputPerMillionUsd: 2 });
    useCompareLabStore.getState().setCostRate(key, null);
    expect(useCompareLabStore.getState().costRates[key]).toBeUndefined();
  });
});

describe("compareLabStore / runs", () => {
  it("createRun seeds a pending result for every (prompt, target) pair and marks it active", () => {
    const suite = emptySuite({
      id: "suite-1",
      prompts: [
        { id: "p1", text: "Prompt 1", toolsEnabled: false, verifier: null, rubricCriteria: [] },
        { id: "p2", text: "Prompt 2", toolsEnabled: true, verifier: null, rubricCriteria: [] },
      ],
    });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a"), target("b")] });

    const run = useCompareLabStore.getState().createRun(suite, modelSet);

    expect(run.status).toBe("running");
    expect(run.results).toHaveLength(4); // 2 prompts x 2 targets
    expect(run.results.every((r) => r.status === "pending")).toBe(true);
    expect(useCompareLabStore.getState().activeRunId).toBe(run.id);
    expect(useCompareLabStore.getState().runs.find((r) => r.id === run.id)).toBeDefined();
  });

  it("updateResult patches the matching (promptId, targetKey) cell without touching others", () => {
    const suite = emptySuite({ id: "suite-1", prompts: [{ id: "p1", text: "Prompt", toolsEnabled: false, verifier: null, rubricCriteria: [] }] });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a"), target("b")] });
    const run = useCompareLabStore.getState().createRun(suite, modelSet);

    useCompareLabStore.getState().updateResult(run.id, "p1", target("a").key, { status: "completed", content: "hi" });

    const updated = useCompareLabStore.getState().runs.find((r) => r.id === run.id);
    const rowA = updated?.results.find((r) => r.targetKey === target("a").key);
    const rowB = updated?.results.find((r) => r.targetKey === target("b").key);
    expect(rowA?.status).toBe("completed");
    expect(rowA?.content).toBe("hi");
    expect(rowB?.status).toBe("pending");
  });

  it("setRubric merges into the existing rubric instead of replacing it", () => {
    const suite = emptySuite({ id: "suite-1", prompts: [{ id: "p1", text: "Prompt", toolsEnabled: false, verifier: null, rubricCriteria: [] }] });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a")] });
    const run = useCompareLabStore.getState().createRun(suite, modelSet);

    useCompareLabStore.getState().setRubric(run.id, "p1", target("a").key, { score: 4 });
    useCompareLabStore.getState().setRubric(run.id, "p1", target("a").key, { notes: "Good" });

    const row = useCompareLabStore
      .getState()
      .runs.find((r) => r.id === run.id)
      ?.results.find((r) => r.targetKey === target("a").key);
    expect(row?.rubric).toEqual({ score: 4, notes: "Good" });
  });

  it("completeRun('cancelled') sweeps every pending/running cell to cancelled but leaves completed cells alone", () => {
    const suite = emptySuite({
      id: "suite-1",
      prompts: [
        { id: "p1", text: "Prompt 1", toolsEnabled: false, verifier: null, rubricCriteria: [] },
        { id: "p2", text: "Prompt 2", toolsEnabled: false, verifier: null, rubricCriteria: [] },
      ],
    });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a")] });
    const run = useCompareLabStore.getState().createRun(suite, modelSet);
    useCompareLabStore.getState().updateResult(run.id, "p1", target("a").key, { status: "completed", content: "done" });

    useCompareLabStore.getState().completeRun(run.id, "cancelled");

    const finished = useCompareLabStore.getState().runs.find((r) => r.id === run.id);
    expect(finished?.status).toBe("cancelled");
    expect(finished?.completedAt).not.toBeNull();
    expect(finished?.results.find((r) => r.promptId === "p1")?.status).toBe("completed");
    expect(finished?.results.find((r) => r.promptId === "p2")?.status).toBe("cancelled");
  });

  it("removeRun drops the run and clears activeRunId only if it pointed at the removed run", () => {
    const suite = emptySuite({ id: "suite-1", prompts: [{ id: "p1", text: "Prompt", toolsEnabled: false, verifier: null, rubricCriteria: [] }] });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a")] });
    const run = useCompareLabStore.getState().createRun(suite, modelSet);

    useCompareLabStore.getState().removeRun(run.id);

    expect(useCompareLabStore.getState().runs.find((r) => r.id === run.id)).toBeUndefined();
    expect(useCompareLabStore.getState().activeRunId).toBeNull();
  });

  it("run history is capped at MAX_RUN_HISTORY, dropping the oldest run first", () => {
    const suite = emptySuite({ id: "suite-1", prompts: [{ id: "p1", text: "Prompt", toolsEnabled: false, verifier: null, rubricCriteria: [] }] });
    const modelSet = emptyModelSet({ id: "set-1", targets: [target("a")] });

    const runs = Array.from({ length: MAX_RUN_HISTORY + 3 }, () => useCompareLabStore.getState().createRun(suite, modelSet));

    const state = useCompareLabStore.getState();
    expect(state.runs).toHaveLength(MAX_RUN_HISTORY);
    // The oldest three are gone; the most recent MAX_RUN_HISTORY remain.
    const keptIds = new Set(state.runs.map((r) => r.id));
    expect(keptIds.has(runs[0].id)).toBe(false);
    expect(keptIds.has(runs[runs.length - 1].id)).toBe(true);
  });

  it("setActiveRun sets and clears the active run id", () => {
    useCompareLabStore.getState().setActiveRun("run-x");
    expect(useCompareLabStore.getState().activeRunId).toBe("run-x");
    useCompareLabStore.getState().setActiveRun(null);
    expect(useCompareLabStore.getState().activeRunId).toBeNull();
  });
});

describe("compareLabStore / emptyResult sanity (shared fixture used by createRun)", () => {
  it("seeds a pending, toolsOffered-tagged cell with no usage/cost/error yet", () => {
    const result: LabResult = emptyResult("p1", "provider:test:a", true);
    expect(result).toMatchObject({
      promptId: "p1",
      targetKey: "provider:test:a",
      status: "pending",
      toolsOffered: true,
      usage: null,
      costUsd: null,
      error: null,
    });
  });
});
