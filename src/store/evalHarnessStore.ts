import { create } from "zustand";

import {
  createEvalCase,
  createEvalSuite,
  createLocalEvalRuntime,
  executeEvalSuite,
  type EvalCase,
  type EvalRun,
  type EvalRuntime,
  type EvalSuite,
} from "../lib/evalHarness";

const STORAGE_KEY = "little-monkey-eval-harness-v1";
const STORAGE_VERSION = 1;
const MAX_RUN_HISTORY = 100;

interface PersistedEvalHarness {
  version: number;
  suites: EvalSuite[];
  runs: EvalRun[];
}

const controllers = new Map<string, AbortController>();
let runtimeFactory: () => EvalRuntime = createLocalEvalRuntime;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function persist(suites: EvalSuite[], runs: EvalRun[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({
      version: STORAGE_VERSION,
      suites,
      runs: runs.slice(0, MAX_RUN_HISTORY),
    } satisfies PersistedEvalHarness));
  } catch {
    // Best effort: the current session remains usable if localStorage is
    // unavailable, full, or disabled.
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === "string");
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isTarget(value: unknown): boolean {
  if (!isObject(value) || typeof value.kind !== "string") return false;
  if (value.kind === "model" || value.kind === "agent") return true;
  if (value.kind === "skill") return typeof value.command === "string";
  if (value.kind === "connector") return typeof value.serverId === "string" && typeof value.toolName === "string";
  return value.kind === "workflow" && typeof value.workflowId === "string";
}

function isExpectation(value: unknown): boolean {
  if (!isObject(value)) return false;
  return isStringArray(value.contains) &&
    (value.regex === null || typeof value.regex === "string") &&
    (value.jsonSubset === null || isObject(value.jsonSubset)) &&
    isStringArray(value.expectedToolCalls) &&
    isStringArray(value.forbiddenToolCalls) &&
    (value.maxLatencyMs === null || typeof value.maxLatencyMs === "number") &&
    (value.maxTotalTokens === null || typeof value.maxTotalTokens === "number") &&
    (value.maxCostMicros === null || typeof value.maxCostMicros === "number");
}

function isCase(value: unknown): value is EvalCase {
  if (!isObject(value)) return false;
  return typeof value.id === "string" && typeof value.name === "string" && typeof value.enabled === "boolean" &&
    typeof value.input === "string" && typeof value.context === "string" &&
    isStringArray(value.retrievalSources) &&
    isStringArray(value.allowedTools) &&
    typeof value.dryRun === "boolean" && ["constraints", "golden", "judge"].includes(String(value.scoringMode)) &&
    isExpectation(value.expectations) && typeof value.goldenAnswer === "string" &&
    typeof value.goldenThreshold === "number" && typeof value.judgeRubric === "string" &&
    typeof value.judgeThreshold === "number";
}

function isSuite(value: unknown): value is EvalSuite {
  if (!isObject(value)) return false;
  return typeof value.id === "string" && typeof value.name === "string" && typeof value.description === "string" &&
    isTarget(value.target) && Array.isArray(value.cases) && value.cases.every(isCase) &&
    typeof value.releaseGate === "boolean" && typeof value.revision === "number" &&
    typeof value.createdAt === "number" && typeof value.updatedAt === "number";
}

function isUsage(value: unknown): boolean {
  return isObject(value) && isFiniteNumber(value.promptTokens) && isFiniteNumber(value.completionTokens) && isFiniteNumber(value.totalTokens);
}

function isEvidence(value: unknown): boolean {
  if (!isObject(value) || typeof value.output !== "string" || !isStringArray(value.toolCalls) ||
    !(value.usage === null || isUsage(value.usage)) || !(value.costMicros === null || isFiniteNumber(value.costMicros)) ||
    typeof value.executionSucceeded !== "boolean" || typeof value.targetLabel !== "string" || !isObject(value.metadata)) return false;
  return Object.values(value.metadata).every((entry) => entry === null || ["string", "number", "boolean"].includes(typeof entry));
}

function isAssertion(value: unknown): boolean {
  return isObject(value) && typeof value.id === "string" && typeof value.label === "string" &&
    typeof value.passed === "boolean" && typeof value.evidence === "string" &&
    ["execution", "verifier", "tool", "latency", "cost", "judge"].includes(String(value.dimension));
}

function isResult(value: unknown): boolean {
  if (!isObject(value)) return false;
  const reproducibility = value.reproducibility;
  return typeof value.caseId === "string" && typeof value.caseName === "string" &&
    ["passed", "failed", "cancelled"].includes(String(value.status)) && typeof value.output === "string" &&
    isStringArray(value.toolCalls) && Array.isArray(value.assertions) && value.assertions.every(isAssertion) &&
    isFiniteNumber(value.latencyMs) && (value.usage === null || isUsage(value.usage)) &&
    (value.costMicros === null || isFiniteNumber(value.costMicros)) && (value.evidence === null || isEvidence(value.evidence)) &&
    (value.error === null || typeof value.error === "string") && isObject(reproducibility) &&
    isFiniteNumber(reproducibility.suiteRevision) && typeof reproducibility.suiteFingerprint === "string" &&
    typeof reproducibility.caseFingerprint === "string" && isTarget(reproducibility.target) &&
    reproducibility.executorVersion === "eval-harness-v1";
}

function isFailureCluster(value: unknown): boolean {
  return isObject(value) && typeof value.key === "string" && typeof value.label === "string" &&
    ["prompt", "model", "connector", "retrieval_source", "verifier", "tool"].includes(String(value.dimension)) &&
    isStringArray(value.caseIds);
}

function isRun(value: unknown): value is EvalRun {
  if (!isObject(value)) return false;
  const basic = typeof value.id === "string" && typeof value.suiteId === "string" && typeof value.suiteName === "string" &&
    typeof value.suiteRevision === "number" && isTarget(value.target) &&
    ["running", "passed", "failed", "cancelled"].includes(String(value.status)) &&
    typeof value.startedAt === "number" && (value.completedAt === null || typeof value.completedAt === "number") &&
    Array.isArray(value.results) && value.results.every(isResult) &&
    Array.isArray(value.failureClusters) && value.failureClusters.every(isFailureCluster) &&
    typeof value.passCount === "number" && typeof value.failCount === "number" &&
    typeof value.totalLatencyMs === "number" && isUsage(value.usage) &&
    (value.costMicros === null || typeof value.costMicros === "number") && typeof value.suiteFingerprint === "string";
  if (!basic) return false;
  const results = value.results as Array<{ status: string }>;
  const passCount = results.filter((result) => result.status === "passed").length;
  const failCount = results.filter((result) => result.status === "failed").length;
  if (value.passCount !== passCount || value.failCount !== failCount) return false;
  if (value.status === "passed" && (results.length === 0 || passCount !== results.length || value.completedAt === null)) return false;
  return true;
}

function hydrate(): { suites: EvalSuite[]; runs: EvalRun[] } {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as unknown;
    if (!isObject(raw) || raw.version !== STORAGE_VERSION || !Array.isArray(raw.suites) || !Array.isArray(raw.runs)) {
      return { suites: [], runs: [] };
    }
    const suites = raw.suites.filter(isSuite);
    const now = Date.now();
    const runs = raw.runs.filter(isRun).map((run) => run.status === "running"
      ? { ...run, status: "cancelled" as const, completedAt: run.completedAt ?? now }
      : run).slice(0, MAX_RUN_HISTORY);
    return { suites, runs };
  } catch {
    return { suites: [], runs: [] };
  }
}

function touch(suite: EvalSuite): EvalSuite {
  return { ...suite, revision: suite.revision + 1, updatedAt: Date.now() };
}

function replaceRun(runs: EvalRun[], run: EvalRun): EvalRun[] {
  return [run, ...runs.filter((candidate) => candidate.id !== run.id)].slice(0, MAX_RUN_HISTORY);
}

const hydrated = hydrate();

export interface EvalHarnessStore {
  suites: EvalSuite[];
  runs: EvalRun[];
  selectedSuiteId: string | null;
  activeRunId: string | null;
  error: string | null;
  selectSuite: (suiteId: string | null) => void;
  createSuite: (name?: string) => string;
  duplicateSuite: (suiteId: string) => string;
  updateSuite: (suiteId: string, patch: Partial<Pick<EvalSuite, "name" | "description" | "target" | "releaseGate">>) => void;
  deleteSuite: (suiteId: string) => void;
  addCase: (suiteId: string) => string;
  duplicateCase: (suiteId: string, caseId: string) => string | null;
  updateCase: (suiteId: string, caseId: string, patch: Partial<Omit<EvalCase, "id">>) => void;
  deleteCase: (suiteId: string, caseId: string) => void;
  runSuite: (suiteId: string) => Promise<EvalRun>;
  cancelRun: () => void;
  clearHistory: (suiteId: string) => void;
  clearError: () => void;
}

export const useEvalHarnessStore = create<EvalHarnessStore>((set, get) => ({
  suites: hydrated.suites,
  runs: hydrated.runs,
  selectedSuiteId: hydrated.suites[0]?.id ?? null,
  activeRunId: null,
  error: null,

  selectSuite: (suiteId) => set({ selectedSuiteId: suiteId, error: null }),

  createSuite: (name) => {
    const suite = createEvalSuite(name?.trim() || "New eval suite");
    const suites = [suite, ...get().suites];
    persist(suites, get().runs);
    set({ suites, selectedSuiteId: suite.id, error: null });
    return suite.id;
  },

  duplicateSuite: (suiteId) => {
    const source = get().suites.find((suite) => suite.id === suiteId);
    if (!source) throw new Error("This eval suite no longer exists.");
    const now = Date.now();
    const suite: EvalSuite = {
      ...structuredClone(source),
      id: crypto.randomUUID(),
      name: `${source.name} copy`,
      cases: source.cases.map((testCase) => ({ ...structuredClone(testCase), id: crypto.randomUUID() })),
      revision: 1,
      releaseGate: false,
      createdAt: now,
      updatedAt: now,
    };
    const suites = [suite, ...get().suites];
    persist(suites, get().runs);
    set({ suites, selectedSuiteId: suite.id, error: null });
    return suite.id;
  },

  updateSuite: (suiteId, patch) => {
    const suites = get().suites.map((suite) => suite.id === suiteId ? touch({ ...suite, ...structuredClone(patch) }) : suite);
    persist(suites, get().runs);
    set({ suites, error: null });
  },

  deleteSuite: (suiteId) => {
    const activeRun = get().activeRunId;
    const activeBelongsToSuite = activeRun && get().runs.find((run) => run.id === activeRun)?.suiteId === suiteId;
    if (activeBelongsToSuite) controllers.get(activeRun)?.abort();
    const suites = get().suites.filter((suite) => suite.id !== suiteId);
    const runs = get().runs.filter((run) => run.suiteId !== suiteId);
    persist(suites, runs);
    set((state) => ({
      suites,
      runs,
      selectedSuiteId: state.selectedSuiteId === suiteId ? suites[0]?.id ?? null : state.selectedSuiteId,
      error: null,
    }));
  },

  addCase: (suiteId) => {
    const testCase = createEvalCase(`Case ${(get().suites.find((suite) => suite.id === suiteId)?.cases.length ?? 0) + 1}`);
    const suites = get().suites.map((suite) => suite.id === suiteId ? touch({ ...suite, cases: [...suite.cases, testCase] }) : suite);
    persist(suites, get().runs);
    set({ suites, error: null });
    return testCase.id;
  },

  duplicateCase: (suiteId, caseId) => {
    const source = get().suites.find((suite) => suite.id === suiteId)?.cases.find((testCase) => testCase.id === caseId);
    if (!source) return null;
    const testCase = { ...structuredClone(source), id: crypto.randomUUID(), name: `${source.name} copy` };
    const suites = get().suites.map((suite) => suite.id === suiteId ? touch({ ...suite, cases: [...suite.cases, testCase] }) : suite);
    persist(suites, get().runs);
    set({ suites, error: null });
    return testCase.id;
  },

  updateCase: (suiteId, caseId, patch) => {
    const suites = get().suites.map((suite) => suite.id === suiteId
      ? touch({ ...suite, cases: suite.cases.map((testCase) => testCase.id === caseId ? { ...testCase, ...structuredClone(patch) } : testCase) })
      : suite);
    persist(suites, get().runs);
    set({ suites, error: null });
  },

  deleteCase: (suiteId, caseId) => {
    const suites = get().suites.map((suite) => suite.id === suiteId
      ? touch({ ...suite, cases: suite.cases.filter((testCase) => testCase.id !== caseId) })
      : suite);
    persist(suites, get().runs);
    set({ suites, error: null });
  },

  runSuite: async (suiteId) => {
    const suite = get().suites.find((candidate) => candidate.id === suiteId);
    if (!suite) throw new Error("This eval suite no longer exists.");
    if (get().activeRunId) throw new Error("Another eval suite is already running.");
    const runId = crypto.randomUUID();
    const controller = new AbortController();
    controllers.set(runId, controller);
    set({ activeRunId: runId, error: null });
    try {
      const run = await executeEvalSuite(
        structuredClone(suite),
        runtimeFactory(),
        controller.signal,
        runId,
        (progress) => {
          set((state) => {
            const runs = replaceRun(state.runs, progress);
            persist(state.suites, runs);
            return { runs };
          });
        },
      );
      set((state) => {
        const runs = replaceRun(state.runs, run);
        persist(state.suites, runs);
        return { runs, activeRunId: null };
      });
      return run;
    } catch (error) {
      set({ activeRunId: null, error: errorText(error) });
      throw error;
    } finally {
      controllers.delete(runId);
    }
  },

  cancelRun: () => {
    const runId = get().activeRunId;
    if (runId) controllers.get(runId)?.abort();
  },

  clearHistory: (suiteId) => {
    const runs = get().runs.filter((run) => run.suiteId !== suiteId || run.id === get().activeRunId);
    persist(get().suites, runs);
    set({ runs });
  },

  clearError: () => set({ error: null }),
}));

/** Test-only dependency seam. Production always uses createLocalEvalRuntime. */
export function __setEvalRuntimeFactoryForTests(factory: (() => EvalRuntime) | null): void {
  runtimeFactory = factory ?? createLocalEvalRuntime;
}

export function __resetEvalHarnessStoreForTests(): void {
  for (const controller of controllers.values()) controller.abort();
  controllers.clear();
  runtimeFactory = createLocalEvalRuntime;
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // no-op
  }
  useEvalHarnessStore.setState({ suites: [], runs: [], selectedSuiteId: null, activeRunId: null, error: null });
}
