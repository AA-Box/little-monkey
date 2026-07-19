import { describe, expect, it, vi } from "vitest";

import {
  clusterEvalFailures,
  createEvalSuite,
  evalFingerprint,
  executeEvalSuite,
  exportEvalRun,
  exportEvalSuite,
  goldenSimilarity,
  releaseGateStatus,
  validateEvalSuite,
  type EvalExecutionEvidence,
  type EvalRuntime,
} from "./evalHarness";

function evidence(overrides: Partial<EvalExecutionEvidence> = {}): EvalExecutionEvidence {
  return {
    output: "The answer is 42.",
    toolCalls: [],
    usage: { promptTokens: 8, completionTokens: 4, totalTokens: 12 },
    costMicros: 3,
    executionSucceeded: true,
    targetLabel: "fixture-model",
    metadata: { fixture: true },
    ...overrides,
  };
}

function runtime(result: EvalExecutionEvidence = evidence()): EvalRuntime {
  return {
    execute: vi.fn().mockResolvedValue(result),
    judge: vi.fn().mockResolvedValue({ passed: true, score: 0.9, evidence: "Meets the rubric.", usage: null }),
  };
}

function runnableSuite() {
  const suite = createEvalSuite("Regression");
  suite.cases[0] = {
    ...suite.cases[0],
    name: "answers correctly",
    input: "What is six times seven?",
    expectations: { ...suite.cases[0].expectations, contains: ["42"] },
  };
  return suite;
}

describe("eval harness", () => {
  it("validates targets, cases, limits, and fail-closed verifier requirements", () => {
    const suite = createEvalSuite("");
    suite.target = { kind: "connector", serverId: "", toolName: "" };
    suite.cases[0].input = "";
    expect(validateEvalSuite(suite).join("\n")).toMatch(/needs at least one/);
    suite.cases[0].expectations.maxLatencyMs = -1;

    const errors = validateEvalSuite(suite).join("\n");
    expect(errors).toMatch(/Suite name/);
    expect(errors).toMatch(/connector server/);
    expect(errors).toMatch(/input is required/);
    expect(errors).toMatch(/non-negative/);
  });

  it("executes cases and derives pass/fail from concrete evidence", async () => {
    const suite = runnableSuite();
    suite.cases[0].allowedTools = ["search"];
    suite.cases[0].expectations.expectedToolCalls = ["search"];
    suite.cases[0].expectations.maxTotalTokens = 12;
    const fixtureRuntime = runtime(evidence({ toolCalls: ["search"] }));

    const run = await executeEvalSuite(suite, fixtureRuntime, new AbortController().signal, "run-1");

    expect(fixtureRuntime.execute).toHaveBeenCalledTimes(1);
    expect(run.status).toBe("passed");
    expect(run.results[0].assertions.map((entry) => entry.id)).toEqual(expect.arrayContaining([
      "execution", "contains-0", "tool-required-search", "tool-allowlist", "tokens",
    ]));
    expect(run.results[0].evidence?.targetLabel).toBe("fixture-model");
    expect(run.usage.totalTokens).toBe(12);
    expect(run.costMicros).toBe(3);
  });

  it("fails on observed output instead of accepting a successful transport", async () => {
    const suite = runnableSuite();
    const run = await executeEvalSuite(suite, runtime(evidence({ output: "I do not know." })), new AbortController().signal);

    expect(run.status).toBe("failed");
    expect(run.results[0].status).toBe("failed");
    expect(run.results[0].assertions.find((entry) => entry.id === "contains-0")?.passed).toBe(false);
  });

  it("derives judge verdict from the numeric score and threshold, not the judge boolean", async () => {
    const suite = runnableSuite();
    suite.cases[0].scoringMode = "judge";
    suite.cases[0].expectations.contains = [];
    suite.cases[0].judgeRubric = "The answer must be correct and concise.";
    suite.cases[0].judgeThreshold = 0.8;
    const fixtureRuntime = runtime();
    fixtureRuntime.judge = vi.fn().mockResolvedValue({ passed: true, score: 0.3, evidence: "Incorrect.", usage: null });

    const run = await executeEvalSuite(suite, fixtureRuntime, new AbortController().signal);

    expect(run.status).toBe("failed");
    expect(run.results[0].assertions.find((entry) => entry.id === "judge")?.passed).toBe(false);
  });

  it("supports golden-answer scoring with a deterministic similarity threshold", async () => {
    const suite = runnableSuite();
    suite.cases[0].expectations.contains = [];
    suite.cases[0].scoringMode = "golden";
    suite.cases[0].goldenAnswer = "The answer is 42";
    suite.cases[0].goldenThreshold = 0.7;

    const run = await executeEvalSuite(suite, runtime(), new AbortController().signal);
    expect(run.status).toBe("passed");
    expect(goldenSimilarity("The answer is 42.", "The answer is 42")).toBe(1);
  });

  it("cancels an in-flight target and records a cancelled result", async () => {
    const suite = runnableSuite();
    const fixtureRuntime = runtime();
    fixtureRuntime.execute = vi.fn((_target, _case, _runId, signal) => new Promise<never>((_, reject) => {
      signal.addEventListener("abort", () => reject(new DOMException("cancelled", "AbortError")), { once: true });
    }));
    const controller = new AbortController();
    const pending = executeEvalSuite(suite, fixtureRuntime, controller.signal);
    controller.abort();

    const run = await pending;
    expect(run.status).toBe("cancelled");
    expect(run.results[0].status).toBe("cancelled");
  });

  it("clusters a failed connector case by prompt, connector, retrieval source, and verifier", async () => {
    const suite = runnableSuite();
    suite.target = { kind: "connector", serverId: "github", toolName: "search" };
    suite.cases[0].input = "{}";
    suite.cases[0].retrievalSources = ["repo-index"];
    suite.cases[0].allowedTools = ["github/search"];
    const run = await executeEvalSuite(
      suite,
      runtime(evidence({ output: "wrong", toolCalls: ["github/search"] })),
      new AbortController().signal,
    );

    const dimensions = new Set(clusterEvalFailures(suite, run.results).map((cluster) => cluster.dimension));
    expect(dimensions).toEqual(new Set(["prompt", "connector", "retrieval_source", "verifier"]));
  });

  it("only opens a release gate from a passing run of the current suite revision", async () => {
    const suite = runnableSuite();
    suite.releaseGate = true;
    const run = await executeEvalSuite(suite, runtime(), new AbortController().signal);

    expect(releaseGateStatus(suite, [run])).toMatchObject({ status: "passed", run: { id: run.id } });
    expect(releaseGateStatus(suite, [{ ...run, passCount: 1, results: [{ ...run.results[0], assertions: [] }] }])).toMatchObject({ status: "blocked" });
    expect(releaseGateStatus({ ...suite, revision: suite.revision + 1 }, [run])).toEqual({ status: "unverified", run: null });
    expect(releaseGateStatus({ ...suite, releaseGate: false }, [run])).toEqual({ status: "not-gated", run: null });
  });

  it("exports reproducible, versioned suite and run artifacts", async () => {
    const suite = runnableSuite();
    const run = await executeEvalSuite(suite, runtime(), new AbortController().signal, "stable-run");
    const suiteExport = JSON.parse(exportEvalSuite(suite));
    const runExport = JSON.parse(exportEvalRun(run));

    expect(suiteExport).toMatchObject({ schemaVersion: 1, kind: "little-monkey-eval-suite" });
    expect(runExport).toMatchObject({ schemaVersion: 1, kind: "little-monkey-eval-run", run: { id: "stable-run" } });
    expect(run.results[0].reproducibility.caseFingerprint).toBe(evalFingerprint(suite.cases[0]));
  });
});
