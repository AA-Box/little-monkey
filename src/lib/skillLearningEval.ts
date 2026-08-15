/**
 * Runs a learning candidate's evaluation through the existing Eval Harness
 * rather than a second, parallel evaluator.
 *
 * The backend produces the plan (cases, required and forbidden tools) and
 * scores the reports. This module only executes the two arms — the candidate's
 * staged instructions, and the same cases with no candidate at all — and hands
 * back what happened. It never decides a verdict, and when no model target is
 * reachable it says so, which the backend records as `unevaluated`.
 */
import {
  createEvalCase,
  createEvalSuite,
  createLocalEvalRuntime,
  executeEvalSuite,
  type EvalCase,
  type EvalRuntime,
  type EvalSuite,
} from "./evalHarness";
import {
  skillLearningClient,
  type EvaluationCaseReport,
  type EvaluationPlan,
  type EvaluationRecord,
} from "./skillLearningClient";

/** Tools offered to both arms. Dry-run only inside the harness: requested
 * calls are captured for scoring and never executed (see `executeModelLike`'s
 * `"dry-run-tool-capture"` mode), which is exactly what the backend's
 * required/forbidden tool contract is scored against. */
function toolsForCase(testCase: { required_tools: string[]; forbidden_tools: string[] }): string[] {
  return [...new Set([...testCase.required_tools, ...testCase.forbidden_tools])].filter((name) =>
    /^[A-Za-z0-9_-]{1,64}$/.test(name),
  );
}

function caseFor(plan: EvaluationPlan, index: number): EvalCase {
  const source = plan.cases[index];
  const testCase = createEvalCase(source.name);
  return {
    ...testCase,
    id: source.case_id,
    input: source.prompt,
    allowedTools: toolsForCase(source),
    expectations: {
      ...testCase.expectations,
      expectedToolCalls: source.required_tools,
      forbiddenToolCalls: source.forbidden_tools,
    },
  };
}

export function suitesForPlan(plan: EvaluationPlan): { baseline: EvalSuite; candidate: EvalSuite } {
  const cases = plan.cases.map((_, index) => caseFor(plan, index));
  const base = createEvalSuite(`Learning candidate ${plan.command}`);
  return {
    baseline: { ...base, id: `${plan.evaluation_id}-baseline`, target: { kind: "model" }, cases },
    candidate: {
      ...base,
      id: `${plan.evaluation_id}-candidate`,
      // The candidate is staged, not installed — the harness takes its
      // instructions inline so it can be exercised before anything is
      // published (see `EvalTarget`'s skill variant).
      target: {
        kind: "skill",
        command: plan.command,
        instructions: plan.skill_instructions,
        allowedTools: plan.allowed_tools,
      },
      cases,
    },
  };
}

/** Maps one harness case result onto the report shape the backend scores. */
function reportFor(
  arm: EvaluationCaseReport["arm"],
  result: {
    caseId: string;
    status: string;
    toolCalls: string[];
    latencyMs: number;
    usage: { promptTokens: number; completionTokens: number } | null;
    costMicros: number | null;
    error: string | null;
  },
): EvaluationCaseReport {
  return {
    case_id: result.caseId,
    arm,
    completed: result.status !== "cancelled" && result.error === null,
    used_tools: result.toolCalls,
    // The harness executes tool calls in dry-run capture mode, so there is no
    // verification result to report. Reported as absent rather than invented.
    verification_passed: null,
    latency_ms: Math.max(0, Math.round(result.latencyMs)),
    input_tokens: result.usage?.promptTokens ?? 0,
    output_tokens: result.usage?.completionTokens ?? 0,
    cost_micros: result.costMicros === null ? null : Math.max(0, Math.round(result.costMicros)),
    permission_requests: [],
    error: result.error,
  };
}

/**
 * Executes both arms and reports them. Returns the backend's own record, so
 * the caller reads the verdict from the store rather than from this module.
 */
export async function runCandidateEvaluation(
  candidateId: string,
  signal: AbortSignal,
  runtime: EvalRuntime = createLocalEvalRuntime(),
  client = skillLearningClient,
): Promise<EvaluationRecord> {
  const plan = await client.planEvaluation(candidateId);
  const { baseline, candidate } = suitesForPlan(plan);
  const reports: EvaluationCaseReport[] = [];
  try {
    const baselineRun = await executeEvalSuite(baseline, runtime, signal, `${plan.evaluation_id}-baseline`);
    reports.push(...baselineRun.results.map((result) => reportFor("baseline", result)));
    const candidateRun = await executeEvalSuite(candidate, runtime, signal, `${plan.evaluation_id}-candidate`);
    reports.push(...candidateRun.results.map((result) => reportFor("candidate", result)));
  } catch (error) {
    // No reachable model target, a cancelled run, or a harness-level failure:
    // all of them mean the candidate was not evaluated. Never a pass.
    return client.markUnevaluated(
      plan.evaluation_id,
      error instanceof Error ? error.message : String(error),
    );
  }
  return client.reportEvaluation(plan.evaluation_id, reports);
}
