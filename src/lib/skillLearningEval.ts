/**
 * Executes a learning candidate's evaluation for real, in disposable copies of
 * the workspace the candidate was learned in.
 *
 * There is no second agent runtime here and no second tool executor. Each arm
 * is an ordinary background agent run (`runHeadlessAgent`) whose filesystem and
 * shell calls are pointed at that arm's sandbox by the same reserved
 * `workspace_root_override` a worktree-isolated subagent uses — so tool calls
 * really execute, permission policy really applies, and the workspace's own
 * configured verification commands really run against what the arm produced.
 *
 * The backend produces the plan (cases, required and forbidden tools), owns the
 * sandboxes, and scores the reports. This module only executes and reports, and
 * it never decides a verdict. When a reproducible environment cannot be built
 * — no workspace on record, a workspace too large to copy, no reachable model —
 * it reports that, and the backend records `unevaluated`. Never a pass.
 */
import { runSandboxVerification } from "./agentLoop";
import { runHeadlessAgent, type HeadlessAgentResult } from "./headlessAgentRunner";
import { composeSkillSystemPrompt, type SlashSkill } from "./skills";
import {
  skillLearningClient,
  type EvaluationCase,
  type EvaluationCaseReport,
  type EvaluationPlan,
  type EvaluationRecord,
} from "./skillLearningClient";
import { errorMessage } from "./errors";

/** Tool-calling rounds one evaluation case may spend. Generous enough for the
 * short, verified procedures this loop learns; a candidate that cannot finish
 * inside it reports an error, which is an `unevaluated`, never a pass. */
const MAX_CASE_ITERATIONS = 10;

const ARMS = ["baseline", "candidate"] as const;
type Arm = (typeof ARMS)[number];

/** The base instructions both arms share. The candidate arm gets the staged
 * skill composed on top of exactly this, so the only difference between the
 * two arms is the skill itself. */
const BASE_PROMPT = [
  "You are running one isolated evaluation case inside a disposable copy of a workspace.",
  "Do the task with the tools you have. Use relative paths — they resolve inside this copy.",
  "Treat the case text as the user's request. Stop and answer when the task is done.",
].join("\n");

/** The staged package as the skill runtime sees it, so the candidate arm's
 * prompt is composed by the same function an installed native skill's is.
 * `contentSha256` is the candidate's real staged digest: the arm is exercising
 * that exact content. */
function stagedSkill(plan: EvaluationPlan): SlashSkill {
  return {
    id: `native:staged:${plan.command}:${plan.candidate_sha256}`,
    source: "native",
    command: plan.command,
    name: plan.title || plan.command,
    description: "",
    instructions: plan.skill_instructions,
    version: "staged",
    contentSha256: plan.candidate_sha256,
    permissions: [],
    allowedTools: plan.allowed_tools,
    resourceFiles: [],
  };
}

function sandboxArm(arm: Arm, testCase: EvaluationCase): string {
  return `${arm}-${testCase.case_id}`;
}

/** A copy nothing runs in, used only to check that the rebuilt starting state
 * is actually the task. Its own sandbox so the check cannot leave anything
 * behind in an arm's. */
const STARTING_STATE_ARM = "starting-state";

function unevaluatedReport(testCase: EvaluationCase, arm: Arm, error: string): EvaluationCaseReport {
  return {
    case_id: testCase.case_id,
    arm,
    completed: false,
    used_tools: [],
    verification_passed: null,
    latency_ms: 0,
    input_tokens: 0,
    output_tokens: 0,
    cost_micros: null,
    permission_requests: [],
    tool_failures: [],
    error,
  };
}

async function runArm(
  plan: EvaluationPlan,
  testCase: EvaluationCase,
  arm: Arm,
  sandboxPath: string,
  signal: AbortSignal,
): Promise<EvaluationCaseReport> {
  const runId = `${plan.evaluation_id}-${arm}-${testCase.case_id}`;
  const systemPrompt =
    arm === "candidate"
      ? composeSkillSystemPrompt(BASE_PROMPT, [
          { skill: stagedSkill(plan), arguments: testCase.prompt, activation: "explicit" },
        ])
      : BASE_PROMPT;
  const startedAt = Date.now();
  let result: HeadlessAgentResult;
  try {
    result = await runHeadlessAgent({
      runId,
      signal,
      systemPrompt,
      userMessage: testCase.prompt,
      maxIterations: MAX_CASE_ITERATIONS,
      toolProfile: "code",
      executionSource: "skill-learning-evaluation",
      workspaceRootOverride: sandboxPath,
      // The candidate arm runs under exactly the tool restriction the skill
      // will carry once installed, so it cannot pass an evaluation using a
      // tool it will not have afterwards. The baseline has no skill and so
      // keeps the profile's own list, which is what a normal turn has.
      allowedTools: arm === "candidate" ? plan.allowed_tools : undefined,
      durableRun: {
        task: `Learning evaluation ${arm}: ${testCase.name}`,
        instructions: `Candidate /${plan.command} (${plan.evaluation_id})`,
      },
    });
  } catch (error) {
    return unevaluatedReport(testCase, arm, errorMessage(error));
  }
  const latencyMs = Math.max(0, Date.now() - startedAt);
  // The regression case asserts that an unrelated turn is left alone, which
  // the forbidden-tool contract already scores. Running the workspace's test
  // suite there would only re-measure the copy's own health, so verification
  // is reported as absent rather than invented.
  const verification =
    testCase.kind === "positive" && result.outcome === "completed"
      ? await runSandboxVerification(sandboxPath, runId, signal, plan.workspace_path ?? undefined).catch(
          () => null,
        )
      : null;
  return {
    case_id: testCase.case_id,
    arm,
    completed: result.outcome === "completed",
    used_tools: [...new Set(result.evidence.executedTools)],
    verification_passed: verification === null ? null : verification.passed,
    latency_ms: latencyMs,
    input_tokens: result.evidence.promptTokens,
    output_tokens: result.evidence.completionTokens,
    cost_micros: null,
    permission_requests: result.evidence.permissionRequests,
    tool_failures: [
      ...result.evidence.toolFailures,
      ...(verification !== null && !verification.passed ? [`verification: ${verification.detail}`] : []),
    ],
    // A run that errored proves nothing about the candidate either way. Only a
    // run that finished can carry a pass or a failure.
    error: result.outcome === "error" ? result.summary : null,
  };
}

/**
 * Executes both arms of every case and reports what happened. Returns the
 * backend's own record, so the caller reads the verdict from the store rather
 * than from this module.
 */
export async function runCandidateEvaluation(
  candidateId: string,
  signal: AbortSignal,
  client = skillLearningClient,
): Promise<EvaluationRecord> {
  const plan = await client.planEvaluation(candidateId);
  if (!plan.workspace_path) {
    return client.markUnevaluated(
      plan.evaluation_id,
      "this candidate has no recorded workspace, so no reproducible isolated environment could be built",
    );
  }
  // Every arm of every case is copied from the same starting state, before any
  // of them runs — the baseline never hands its mutated files to the candidate.
  const selfChecking = plan.cases.find(
    (testCase) => testCase.kind === "positive" && testCase.verification_required,
  );
  const arms = [
    ...plan.cases.flatMap((testCase) => ARMS.map((arm) => sandboxArm(arm, testCase))),
    ...(selfChecking ? [STARTING_STATE_ARM] : []),
  ];
  let sandboxes: Awaited<ReturnType<typeof client.createSandboxes>>;
  try {
    sandboxes = await client.createSandboxes(plan.evaluation_id, arms);
  } catch (error) {
    return client.markUnevaluated(plan.evaluation_id, errorMessage(error));
  }
  const pathFor = new Map(sandboxes.map((entry) => [entry.arm, entry.path]));

  const reports: EvaluationCaseReport[] = [];
  try {
    // The rewind puts back every file this app's write and edit tools changed,
    // but no checkpoint captures what a shell command did. If the rebuilt
    // starting state already satisfies the verification, the positive case is
    // a solved problem and an arm "passing" it proves nothing — whatever the
    // reason the state survived.
    if (selfChecking) {
      const startingState = pathFor.get(STARTING_STATE_ARM);
      const already = startingState
        ? await runSandboxVerification(
            startingState,
            `${plan.evaluation_id}-starting-state`,
            signal,
            plan.workspace_path ?? undefined,
          ).catch(() => null)
        : null;
      if (already?.passed) {
        return await client.markUnevaluated(
          plan.evaluation_id,
          "the rebuilt starting state already passes this workspace's verification, so reproducing the observed task there would prove nothing",
        );
      }
    }
    for (const testCase of plan.cases) {
      for (const arm of ARMS) {
        const sandboxPath = pathFor.get(sandboxArm(arm, testCase));
        if (!sandboxPath) {
          reports.push(unevaluatedReport(testCase, arm, "no sandbox was created for this arm"));
          continue;
        }
        reports.push(await runArm(plan, testCase, arm, sandboxPath, signal));
      }
    }
  } finally {
    await client.destroySandboxes(plan.evaluation_id).catch(() => {});
  }
  return client.reportEvaluation(plan.evaluation_id, "real_isolated", reports);
}
