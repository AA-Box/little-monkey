import { create } from "zustand";

import {
  BENCHMARK_CATEGORIES,
  buildStarterSuites,
  createLabPromptId,
  emptyResult,
  type BenchmarkCategory,
  type BenchmarkSuite,
  type LabCostRate,
  type LabPrompt,
  type LabResult,
  type LabRubric,
  type LabRun,
  type LabVerifier,
  type LabVerifierKind,
  type ModelSet,
} from "../lib/compareLab";
import { isModelTargetSnapshot, type ModelTargetSnapshot } from "../lib/modelTargets";

/** localStorage key the whole Compare Lab blob (suites, model sets, cost
 * rates, run history) is persisted under — same standalone-store pattern as
 * `settingsStore.ts`'s `STORAGE_KEY`, deliberately NOT folded into
 * `sessionStore.ts`'s Tauri-file-backed session blob: saved suites/model
 * sets are reusable presets independent of any one chat session or window,
 * closer in spirit to saved Settings than to a conversation. */
export const COMPARE_LAB_STORAGE_KEY = "little-monkey-compare-lab";

/** Run history is capped so the persisted blob (which embeds full prompt
 * snapshots and every response's full text) can't grow without bound —
 * oldest runs are dropped first. */
export const MAX_RUN_HISTORY = 20;

interface PersistedShape {
  version: 1;
  suites: BenchmarkSuite[];
  modelSets: ModelSet[];
  costRates: Record<string, LabCostRate>;
  runs: LabRun[];
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

const VERIFIER_KINDS: readonly LabVerifierKind[] = ["contains", "not_contains", "regex", "json_valid", "min_length"];

function isValidVerifier(value: unknown): value is LabVerifier {
  if (value === null) return true;
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<LabVerifier>;
  return (
    VERIFIER_KINDS.includes(candidate.kind as LabVerifierKind) &&
    (candidate.value === undefined || typeof candidate.value === "string") &&
    (candidate.flags === undefined || typeof candidate.flags === "string") &&
    typeof candidate.label === "string"
  );
}

function isValidPrompt(value: unknown): value is LabPrompt {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<LabPrompt>;
  return (
    isNonEmptyString(candidate.id) &&
    typeof candidate.text === "string" &&
    typeof candidate.toolsEnabled === "boolean" &&
    isValidVerifier(candidate.verifier ?? null) &&
    Array.isArray(candidate.rubricCriteria) &&
    candidate.rubricCriteria.every((entry) => typeof entry === "string")
  );
}

function isValidSuite(value: unknown): value is BenchmarkSuite {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<BenchmarkSuite>;
  return (
    isNonEmptyString(candidate.id) &&
    isNonEmptyString(candidate.name) &&
    typeof candidate.description === "string" &&
    BENCHMARK_CATEGORIES.includes(candidate.category as BenchmarkCategory) &&
    Array.isArray(candidate.prompts) &&
    candidate.prompts.every(isValidPrompt) &&
    typeof candidate.builtIn === "boolean" &&
    isFiniteNumber(candidate.createdAt) &&
    isFiniteNumber(candidate.updatedAt)
  );
}

function isValidModelSet(value: unknown): value is ModelSet {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<ModelSet>;
  return (
    isNonEmptyString(candidate.id) &&
    isNonEmptyString(candidate.name) &&
    Array.isArray(candidate.targets) &&
    candidate.targets.every(isModelTargetSnapshot) &&
    isFiniteNumber(candidate.createdAt) &&
    isFiniteNumber(candidate.updatedAt)
  );
}

function isValidCostRate(value: unknown): value is LabCostRate {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<LabCostRate>;
  return (
    isFiniteNumber(candidate.inputPerMillionUsd) &&
    candidate.inputPerMillionUsd >= 0 &&
    isFiniteNumber(candidate.outputPerMillionUsd) &&
    candidate.outputPerMillionUsd >= 0
  );
}

function sanitizeCostRates(value: unknown): Record<string, LabCostRate> {
  if (!value || typeof value !== "object") return {};
  const out: Record<string, LabCostRate> = {};
  for (const [key, rate] of Object.entries(value as Record<string, unknown>)) {
    if (isValidCostRate(rate)) out[key] = rate;
  }
  return out;
}

function isValidRun(value: unknown): value is LabRun {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<LabRun>;
  return (
    isNonEmptyString(candidate.id) &&
    isNonEmptyString(candidate.suiteId) &&
    typeof candidate.suiteName === "string" &&
    BENCHMARK_CATEGORIES.includes(candidate.suiteCategory as BenchmarkCategory) &&
    isNonEmptyString(candidate.modelSetId) &&
    typeof candidate.modelSetName === "string" &&
    Array.isArray(candidate.prompts) &&
    candidate.prompts.every(isValidPrompt) &&
    Array.isArray(candidate.targets) &&
    candidate.targets.every(isModelTargetSnapshot) &&
    isFiniteNumber(candidate.createdAt) &&
    (candidate.completedAt === null || isFiniteNumber(candidate.completedAt)) &&
    (candidate.status === "running" || candidate.status === "completed" || candidate.status === "cancelled") &&
    Array.isArray(candidate.results)
  );
}

function defaults(): PersistedShape {
  return { version: 1, suites: buildStarterSuites(), modelSets: [], costRates: {}, runs: [] };
}

function hydrate(): PersistedShape {
  const fallback = defaults();
  try {
    const raw = localStorage.getItem(COMPARE_LAB_STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return fallback;
    const suites = Array.isArray(parsed.suites) ? parsed.suites.filter(isValidSuite) : fallback.suites;
    return {
      version: 1,
      // A blob that lost every suite (corrupt/hand-edited) falls back to the
      // starter set rather than leaving the Lab with nothing runnable.
      suites: suites.length > 0 ? suites : fallback.suites,
      modelSets: Array.isArray(parsed.modelSets) ? parsed.modelSets.filter(isValidModelSet) : [],
      costRates: sanitizeCostRates(parsed.costRates),
      runs: (Array.isArray(parsed.runs) ? parsed.runs.filter(isValidRun) : []).slice(-MAX_RUN_HISTORY),
    };
  } catch {
    return fallback;
  }
}

function persist(state: PersistedShape): void {
  try {
    localStorage.setItem(
      COMPARE_LAB_STORAGE_KEY,
      JSON.stringify({ ...state, runs: state.runs.slice(-MAX_RUN_HISTORY) }),
    );
  } catch {
    // Best-effort — persistence must never throw into a caller mid-run.
  }
}

export interface CompareLabStore {
  suites: BenchmarkSuite[];
  modelSets: ModelSet[];
  costRates: Record<string, LabCostRate>;
  runs: LabRun[];
  activeRunId: string | null;

  saveSuite: (suite: BenchmarkSuite) => string;
  removeSuite: (id: string) => void;
  duplicateSuite: (id: string) => string | null;

  saveModelSet: (name: string, targets: readonly ModelTargetSnapshot[], id?: string) => string;
  removeModelSet: (id: string) => void;

  setCostRate: (targetKey: string, rate: LabCostRate | null) => void;

  createRun: (suite: BenchmarkSuite, modelSet: ModelSet) => LabRun;
  updateResult: (runId: string, promptId: string, targetKey: string, patch: Partial<LabResult>) => void;
  setRubric: (runId: string, promptId: string, targetKey: string, rubric: Partial<LabRubric>) => void;
  completeRun: (runId: string, status: "completed" | "cancelled") => void;
  removeRun: (runId: string) => void;
  setActiveRun: (runId: string | null) => void;
}

function normalizePrompt(prompt: LabPrompt): LabPrompt {
  return {
    id: isNonEmptyString(prompt.id) ? prompt.id : createLabPromptId(),
    text: typeof prompt.text === "string" ? prompt.text : "",
    toolsEnabled: prompt.toolsEnabled === true,
    verifier: isValidVerifier(prompt.verifier) ? prompt.verifier : null,
    rubricCriteria: Array.isArray(prompt.rubricCriteria) ? prompt.rubricCriteria.filter((c) => typeof c === "string") : [],
  };
}

export const useCompareLabStore = create<CompareLabStore>((set, get) => {
  const initial = hydrate();

  return {
    suites: initial.suites,
    modelSets: initial.modelSets,
    costRates: initial.costRates,
    runs: initial.runs,
    activeRunId: null,

    saveSuite: (suite) => {
      const id = isNonEmptyString(suite.id) ? suite.id : crypto.randomUUID();
      const now = Date.now();
      set((state) => {
        const normalized: BenchmarkSuite = {
          ...suite,
          id,
          prompts: suite.prompts.map(normalizePrompt),
          updatedAt: now,
          createdAt: state.suites.find((s) => s.id === id)?.createdAt ?? suite.createdAt ?? now,
        };
        const exists = state.suites.some((s) => s.id === id);
        const suites = exists
          ? state.suites.map((s) => (s.id === id ? normalized : s))
          : [...state.suites, normalized];
        persist({ version: 1, suites, modelSets: state.modelSets, costRates: state.costRates, runs: state.runs });
        return { suites };
      });
      return id;
    },

    removeSuite: (id) => {
      set((state) => {
        const suites = state.suites.filter((s) => s.id !== id);
        persist({ version: 1, suites, modelSets: state.modelSets, costRates: state.costRates, runs: state.runs });
        return { suites };
      });
    },

    duplicateSuite: (id) => {
      const source = get().suites.find((s) => s.id === id);
      if (!source) return null;
      const now = Date.now();
      const clone: BenchmarkSuite = {
        ...source,
        id: crypto.randomUUID(),
        name: `${source.name} (copy)`,
        builtIn: false,
        prompts: source.prompts.map((prompt) => ({ ...prompt, id: createLabPromptId() })),
        createdAt: now,
        updatedAt: now,
      };
      set((state) => {
        const suites = [...state.suites, clone];
        persist({ version: 1, suites, modelSets: state.modelSets, costRates: state.costRates, runs: state.runs });
        return { suites };
      });
      return clone.id;
    },

    saveModelSet: (name, targets, id) => {
      const setId = id ?? crypto.randomUUID();
      const now = Date.now();
      set((state) => {
        const normalized: ModelSet = {
          id: setId,
          name: name.trim() || "Untitled set",
          targets: structuredClone([...targets]),
          createdAt: state.modelSets.find((s) => s.id === setId)?.createdAt ?? now,
          updatedAt: now,
        };
        const exists = state.modelSets.some((s) => s.id === setId);
        const modelSets = exists
          ? state.modelSets.map((s) => (s.id === setId ? normalized : s))
          : [...state.modelSets, normalized];
        persist({ version: 1, suites: state.suites, modelSets, costRates: state.costRates, runs: state.runs });
        return { modelSets };
      });
      return setId;
    },

    removeModelSet: (id) => {
      set((state) => {
        const modelSets = state.modelSets.filter((s) => s.id !== id);
        persist({ version: 1, suites: state.suites, modelSets, costRates: state.costRates, runs: state.runs });
        return { modelSets };
      });
    },

    setCostRate: (targetKey, rate) => {
      set((state) => {
        const costRates = { ...state.costRates };
        if (rate === null) delete costRates[targetKey];
        else costRates[targetKey] = rate;
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates, runs: state.runs });
        return { costRates };
      });
    },

    createRun: (suite, modelSet) => {
      const prompts = structuredClone(suite.prompts);
      const targets = structuredClone(modelSet.targets);
      // Every (prompt, target) cell is seeded up front as "pending" so the
      // report grid is always complete — including cells a Stop request
      // never got to start — rather than only ever containing rows the
      // runner happened to reach.
      const results: LabResult[] = prompts.flatMap((prompt) =>
        targets.map((target) => emptyResult(prompt.id, target.key, prompt.toolsEnabled)),
      );
      const run: LabRun = {
        id: crypto.randomUUID(),
        suiteId: suite.id,
        suiteName: suite.name,
        suiteCategory: suite.category,
        modelSetId: modelSet.id,
        modelSetName: modelSet.name,
        prompts,
        targets,
        createdAt: Date.now(),
        completedAt: null,
        status: "running",
        results,
      };
      set((state) => {
        const runs = [...state.runs, run].slice(-MAX_RUN_HISTORY);
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates: state.costRates, runs });
        return { runs, activeRunId: run.id };
      });
      return run;
    },

    updateResult: (runId, promptId, targetKey, patch) => {
      set((state) => {
        const runs = state.runs.map((run) => {
          if (run.id !== runId) return run;
          const index = run.results.findIndex((r) => r.promptId === promptId && r.targetKey === targetKey);
          const results = [...run.results];
          if (index === -1) {
            // Result rows are seeded lazily by the runner right before it
            // starts each pair — an unknown (promptId, targetKey) is folded
            // in as a fresh row rather than silently dropped.
            results.push({ ...patch } as LabResult);
          } else {
            results[index] = { ...results[index], ...patch };
          }
          return { ...run, results };
        });
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates: state.costRates, runs });
        return { runs };
      });
    },

    setRubric: (runId, promptId, targetKey, rubric) => {
      set((state) => {
        const runs = state.runs.map((run) => {
          if (run.id !== runId) return run;
          const results = run.results.map((result) =>
            result.promptId === promptId && result.targetKey === targetKey
              ? { ...result, rubric: { ...result.rubric, ...rubric } }
              : result,
          );
          return { ...run, results };
        });
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates: state.costRates, runs });
        return { runs };
      });
    },

    completeRun: (runId, status) => {
      set((state) => {
        const completedAt = Date.now();
        const runs = state.runs.map((run) => {
          if (run.id !== runId) return run;
          // A cancelled run may still have cells the runner never reached
          // (seeded "pending") or aborted mid-stream ("running") — both are
          // swept to "cancelled" here so the report never shows a stale
          // in-progress state for a run that has actually stopped.
          const results =
            status === "cancelled"
              ? run.results.map((result) =>
                  result.status === "pending" || result.status === "running"
                    ? { ...result, status: "cancelled" as const, completedAt: result.completedAt ?? completedAt }
                    : result,
                )
              : run.results;
          return { ...run, status, completedAt, results };
        });
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates: state.costRates, runs });
        return { runs };
      });
    },

    removeRun: (runId) => {
      set((state) => {
        const runs = state.runs.filter((run) => run.id !== runId);
        persist({ version: 1, suites: state.suites, modelSets: state.modelSets, costRates: state.costRates, runs });
        return { runs, activeRunId: state.activeRunId === runId ? null : state.activeRunId };
      });
    },

    setActiveRun: (runId) => set({ activeRunId: runId }),
  };
});
