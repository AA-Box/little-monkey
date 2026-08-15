/**
 * What the isolated evaluator is allowed to report.
 *
 * The rules that matter here are the ones a passing evaluation rests on: every
 * arm starts from its own copy made before any arm runs, only tools that
 * actually executed are reported as used, a real verification result is
 * carried through rather than invented, and an environment that could not be
 * built is `unevaluated` rather than a pass.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const runHeadlessAgent = vi.fn();
const runSandboxVerification = vi.fn();

vi.mock("./headlessAgentRunner", () => ({ runHeadlessAgent: (...args: unknown[]) => runHeadlessAgent(...args) }));
vi.mock("./agentLoop", () => ({ runSandboxVerification: (...args: unknown[]) => runSandboxVerification(...args) }));

import { runCandidateEvaluation } from "./skillLearningEval";
import type { EvaluationCaseReport, EvaluationPlan } from "./skillLearningClient";

function plan(overrides: Partial<EvaluationPlan> = {}): EvaluationPlan {
  return {
    evaluation_id: "eval-abc",
    candidate_id: "learn-1",
    command: "retry-wrapper",
    title: "Retry wrapper",
    candidate_sha256: "a".repeat(64),
    skill_instructions: "Wrap the call in withRetry.",
    allowed_tools: ["read_file", "edit_file"],
    cases: [
      {
        case_id: "positive",
        kind: "positive",
        name: "Reproduces the observed task",
        prompt: "wrap the uploader",
        required_tools: ["edit_file"],
        forbidden_tools: [],
        verification_required: true,
      },
      {
        case_id: "regression",
        kind: "regression",
        name: "Leaves an unrelated turn alone",
        prompt: "Reply with OK.",
        required_tools: [],
        forbidden_tools: ["edit_file"],
        verification_required: false,
      },
    ],
    workspace_path: "/tmp/workspace",
    ...overrides,
  };
}

function client(overrides: Record<string, unknown> = {}) {
  return {
    planEvaluation: vi.fn(async () => plan()),
    createSandboxes: vi.fn(async (_id: string, arms: string[]) =>
      arms.map((arm) => ({ arm, path: `/sandboxes/${arm}` })),
    ),
    destroySandboxes: vi.fn(async () => undefined),
    reportEvaluation: vi.fn(async () => ({ verdict: "passed" })),
    markUnevaluated: vi.fn(async () => ({ verdict: "unevaluated" })),
    ...overrides,
  };
}

beforeEach(() => {
  runHeadlessAgent.mockReset();
  runSandboxVerification.mockReset();
  runHeadlessAgent.mockResolvedValue({
    outcome: "completed",
    summary: "done",
    durableRunId: "run-1",
    evidence: {
      executedTools: ["read_file", "edit_file"],
      toolFailures: [],
      permissionRequests: [],
      promptTokens: 10,
      completionTokens: 5,
    },
  });
  runSandboxVerification.mockResolvedValue({ passed: true, detail: "1 command passed" });
});

describe("runCandidateEvaluation", () => {
  it("reports a real isolated run, with the verification result it actually got", async () => {
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);

    const [, mode, reports] = api.reportEvaluation.mock.calls[0] as unknown as [
      string,
      string,
      EvaluationCaseReport[],
    ];
    expect(mode).toBe("real_isolated");
    const positive = reports.find((entry) => entry.case_id === "positive" && entry.arm === "candidate");
    expect(positive).toMatchObject({
      completed: true,
      used_tools: ["read_file", "edit_file"],
      // A real result, not the `null` a capture-only run has to report.
      verification_passed: true,
      input_tokens: 10,
      output_tokens: 5,
    });
    expect(api.destroySandboxes).toHaveBeenCalledWith("eval-abc");
  });

  it("gives every arm its own copy, all created before any arm runs", async () => {
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);

    const [, arms] = api.createSandboxes.mock.calls[0] as unknown as [string, string[]];
    expect(arms).toEqual([
      "baseline-positive",
      "candidate-positive",
      "baseline-regression",
      "candidate-regression",
    ]);
    // One call, before the first arm ran: the baseline never hands its
    // mutated files to the candidate.
    expect(api.createSandboxes).toHaveBeenCalledTimes(1);
    const sandboxes = runHeadlessAgent.mock.calls.map(
      (call: unknown[]) => (call[0] as { workspaceRootOverride: string }).workspaceRootOverride,
    );
    expect(new Set(sandboxes).size).toBe(4);
    expect(sandboxes.every((path: string) => path.startsWith("/sandboxes/"))).toBe(true);
  });

  it("puts the staged skill in front of the candidate arm only", async () => {
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    const prompts = runHeadlessAgent.mock.calls.map(
      (call: unknown[]) => (call[0] as { systemPrompt: string }).systemPrompt,
    );
    const withSkill = prompts.filter((prompt: string) => prompt.includes("withRetry"));
    expect(withSkill).toHaveLength(2);
    // The two arms are otherwise identical, so the skill is the only variable.
    expect(prompts.filter((prompt: string) => !prompt.includes("withRetry"))).toHaveLength(2);
  });

  it("runs the candidate arm under the restriction the skill will carry once installed", async () => {
    // Otherwise an arm could pass using a tool the installed skill will not
    // have, and the evaluation would not be measuring what ships.
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    const byArm = new Map(
      runHeadlessAgent.mock.calls.map((call: unknown[]) => {
        const params = call[0] as { runId: string; allowedTools?: string[] };
        return [params.runId, params.allowedTools];
      }),
    );
    expect(byArm.get("eval-abc-candidate-positive")).toEqual(["read_file", "edit_file"]);
    // The baseline has no skill, so it has the profile's own tools — exactly
    // what an ordinary turn has, which is what it is standing in for.
    expect(byArm.get("eval-abc-baseline-positive")).toBeUndefined();
  });

  it("is unevaluated, never a pass, when no reproducible environment exists", async () => {
    const api = client({ planEvaluation: vi.fn(async () => plan({ workspace_path: null })) });
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    expect(api.markUnevaluated).toHaveBeenCalledTimes(1);
    expect(api.reportEvaluation).not.toHaveBeenCalled();
    expect(runHeadlessAgent).not.toHaveBeenCalled();
  });

  it("is unevaluated when the workspace cannot be copied", async () => {
    const api = client({
      createSandboxes: vi.fn(async () => {
        throw new Error("the workspace is too large to copy for evaluation");
      }),
    });
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    const [, reason] = api.markUnevaluated.mock.calls[0] as unknown as [string, string];
    expect(reason).toContain("too large");
    expect(api.reportEvaluation).not.toHaveBeenCalled();
  });

  it("reports an arm that could not run as an error rather than a failure", async () => {
    runHeadlessAgent.mockResolvedValue({
      outcome: "error",
      summary: "no model target is configured",
      durableRunId: null,
      evidence: {
        executedTools: [],
        toolFailures: [],
        permissionRequests: [],
        promptTokens: 0,
        completionTokens: 0,
      },
    });
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    const [, , reports] = api.reportEvaluation.mock.calls[0] as unknown as [
      string,
      string,
      EvaluationCaseReport[],
    ];
    // The backend scores an errored candidate arm as `unevaluated`; nothing
    // here decides that, but it must report the error rather than a bare
    // "did not complete", which would score as a failure.
    expect(reports[1].error).toContain("no model target");
    expect(reports[1].verification_passed).toBeNull();
  });

  it("does not claim a verification the regression case never ran", async () => {
    const api = client();
    await runCandidateEvaluation("learn-1", new AbortController().signal, api as never);
    const [, , reports] = api.reportEvaluation.mock.calls[0] as unknown as [
      string,
      string,
      EvaluationCaseReport[],
    ];
    const regression = reports.find((entry) => entry.case_id === "regression");
    expect(regression?.verification_passed).toBeNull();
    expect(runSandboxVerification).toHaveBeenCalledTimes(2);
  });
});
