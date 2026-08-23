/**
 * The Issue-to-PR Agent Flow's headless "implement" phase — drives a REAL
 * agent turn (not a scripted templated diff) against the owned worktree
 * `issue_to_pr.rs` already created, using the exact same primitives a normal
 * chat turn or a `task`-tool subagent uses: `turnEngine.ts`'s
 * `attemptStream`/`executeToolCall`, `tools.ts`'s `toolsForProfile('code')`,
 * and (for evidence) the same Run Capsule ledger every other run already
 * writes to (`durableRun.ts`'s `beginDurableRun`, `kind: 'background'` — the
 * existing run kind for a non-chat-bubble, panel-driven run).
 *
 * Structurally this is `subagent.ts`'s `runSubagentTask` adapted for a
 * headless/panel-driven run rather than a delegated child of an interactive
 * turn: same model->tools->model loop shape, same permission-gated tool
 * dispatch (so write_file/edit_file/run_shell still prompt exactly like any
 * other agent-initiated mutation — nothing here bypasses that), but reporting
 * progress to `issueToPrStore.ts` instead of `subagentStore.ts`, and with no
 * parent turn to inherit a checkpoint id or risk-annotation context from.
 *
 * All file/shell paths the model uses MUST be prefixed with the owned
 * worktree's attached secondary-workspace-root label (see `workspace.rs`'s
 * `resolve_path_and_root` doc comment) — the system prompt built here is the
 * only thing enforcing that convention on the model side; the actual
 * sandboxing (a path outside that root is rejected) is enforced by Rust
 * regardless of what the model does.
 */
import { runHeadlessAgent } from './headlessAgentRunner';
import { wrapUntrustedContent } from './untrustedContent';
import { defaultAutonomousTaskRuntime, startAutonomousTask, type AutonomousTaskRuntime, type StructuredReviewResult } from './autonomousTaskRunner';
import type { AutonomousTask, TaskDeliveryTarget } from './autonomousTask';
import { resolveTarget } from './targetRouting';
import { inspectOwnedWorktree } from './gitDelivery';
import { runIssueToPrChecks } from './issueToPr';

/** Hard cap on model/tool round trips — generous relative to
 * `subagent.ts`'s `MAX_SUBAGENT_ITERATIONS` (15) since a full issue
 * implementation (read around, edit, run tests, fix, re-run) is a much
 * larger task than a single delegated subtask. */
export const MAX_ISSUE_TO_PR_ITERATIONS = 40;

export interface RunIssueToPrAgentParams {
  /** Reused as both the headless loop's own `turnId` (scoping Rust-side
   * permission prompts/cancellation) and the Run Capsule ledger's `run_id` —
   * one id, one evidence trail, exactly one issue-to-pr run. */
  runId: string;
  repositorySlug: string;
  issueNumber: number;
  issueTitle: string;
  issueBody: string;
  branch: string;
  baseRef: string;
  worktreeId?: string;
  /** The secondary workspace root label `issue_to_pr.rs` attached for this
   * run's owned worktree — every tool path the model uses must be prefixed
   * with `"<label>/"` to land inside it. */
  workspaceLabel: string;
  signal: AbortSignal;
  /** Called once per tool call the model makes, purely for the panel's live
   * "current activity" line — never gates anything. */
  onToolActivity?: (label: string) => void;
}

export interface IssueToPrAgentResult {
  outcome: 'completed' | 'cancelled' | 'error';
  /** The agent's own final summary (or an error/cancellation message). */
  summary: string;
  /** The Run Capsule ledger id this run was recorded under, if the desktop
   * host's run-protocol version matched (see `beginDurableRun`'s doc
   * comment) — `null` on an older host, where the flow still runs but has
   * no capsule to show. */
  durableRunId: string | null;
}

function parseStructuredReview(value: string): StructuredReviewResult | null {
  const candidate = value.match(/\{[\s\S]*\}/)?.[0];
  if (!candidate) return null;
  try {
    const parsed = JSON.parse(candidate) as Partial<StructuredReviewResult>;
    if (parsed.verdict !== 'pass' && parsed.verdict !== 'changes_required') return null;
    if (!Array.isArray(parsed.findings) || !Array.isArray(parsed.filesReviewed) || !Array.isArray(parsed.acceptanceCriteria) || !Array.isArray(parsed.securityFindings) || !Array.isArray(parsed.testCoverageFindings)) return null;
    return parsed as StructuredReviewResult;
  } catch { return null; }
}

async function inspectionRevision(inspection: Awaited<ReturnType<typeof inspectOwnedWorktree>>): Promise<string> {
  const material = JSON.stringify({ headOid: inspection.headOid, files: inspection.files, staged: inspection.diffs.staged.text, unstaged: inspection.diffs.unstaged.text, head: inspection.diffs.head.text });
  const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(material)))).map((byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${inspection.headOid}:${digest}`;
}

/** Issue-to-PR's coordinator adapter. The existing state machine remains the
 * owner of GitHub delivery and its confirmation phrase; this adapter gives
 * the implementation phase the same plan/worker/evidence contract as every
 * other autonomous task without moving external mutations into the worker. */
export async function runIssueToPrAutonomousTask(
  params: RunIssueToPrAgentParams & { sessionId?: string },
): Promise<IssueToPrAgentResult & { task: AutonomousTask }> {
  let implementation: IssueToPrAgentResult | null = null;
  const resolvedTarget = await resolveTarget();
  const baseRuntime = defaultAutonomousTaskRuntime(resolvedTarget);
  const deliveryTarget: TaskDeliveryTarget = { worktreeId: params.worktreeId ?? params.runId, repositorySlug: params.repositorySlug, branch: params.branch, remote: 'origin', base: params.baseRef, title: `Issue #${params.issueNumber}: ${params.issueTitle}`, body: `Implements GitHub issue #${params.issueNumber}.` };
  const runtime: AutonomousTaskRuntime = {
    ...baseRuntime,
    executeNode: async (task, node, context) => {
      if (node.taskClass !== 'implementation') return { ok: true, summary: `Prepared ${node.taskClass} context.` };
      const before = await inspectOwnedWorktree(deliveryTarget.worktreeId);
      implementation = await runIssueToPrAgent({ ...params, runId: params.runId, signal: context.signal });
      const after = await inspectOwnedWorktree(deliveryTarget.worktreeId);
      const beforeRevision = await inspectionRevision(before);
      const afterRevision = await inspectionRevision(after);
      return { ok: implementation.outcome === 'completed', summary: implementation.summary, changedFiles: after.files.map((file) => file.path), workspaceRevision: afterRevision, mutation: { beforeRevision: task.workspaceRevision ?? beforeRevision, afterRevision, changedFiles: after.files.map((file) => file.path), patchDigest: afterRevision } };
    },
    integrate: async (task) => {
      const inspection = await inspectOwnedWorktree(deliveryTarget.worktreeId);
      const changedFiles = inspection.files.map((file) => file.path);
      const afterRevision = await inspectionRevision(inspection);
      return { ok: true, summary: 'The issue flow retains its owned worktree; no shared workspace mutation is performed.', changedFiles, workspaceRevision: afterRevision, mutation: { beforeRevision: task.workspaceRevision ?? afterRevision, afterRevision, changedFiles, patchDigest: afterRevision } };
    },
    verify: async (current) => {
      if (implementation?.outcome !== 'completed') return { ok: false, summary: implementation?.summary ?? 'Implementation did not complete.' };
      const checked = await runIssueToPrChecks(params.runId);
      const passed = checked.checks.length > 0 && checked.checks.every((check) => check.passed);
      const criterion = current.acceptanceCriteria.find((candidate) => candidate.method === 'verification_command');
      return { ok: passed, summary: passed ? 'Issue repository checks passed.' : checked.checks.map((check) => `${check.label}: ${check.outputExcerpt}`).join('\n') || 'No repository checks were configured.', evidence: [{ evidenceId: `issue-checks-${params.runId}`, criterionId: criterion?.id ?? null, name: 'Issue-to-PR checks', passed, authoritative: true, stale: false, summary: checked.checks.map((check) => `${check.label}: ${check.outputExcerpt}`).join('\n'), exitCode: passed ? 0 : 1, durationMs: 0, createdAtMs: Date.now(), source: 'command' }] };
    },
    review: async (current, _node, context) => {
      const reviewRun = await runHeadlessAgent({ runId: `${params.runId}-review`, signal: context.signal, systemPrompt: 'Review the owned worktree diff for the stated issue. Return strict JSON only: {"verdict":"pass"|"changes_required","findings":[{"severity":"blocking"|"warning"|"suggestion","path":string,"title":string,"body":string}],"filesReviewed":string[],"acceptanceCriteria":string[],"securityFindings":string[],"testCoverageFindings":string[]}. Do not mutate files.', userMessage: `Issue #${params.issueNumber}: ${params.issueTitle}\n${wrapUntrustedContent('Issue body', params.issueBody)}\nReview the implementation and checks in the app-owned worktree ${params.workspaceLabel}.`, maxIterations: 8, executionSource: 'issue-to-pr-review', durableRun: { task: `Review issue-to-PR #${params.issueNumber}`, instructions: `Owned branch ${params.branch}` } });
      const review = parseStructuredReview(reviewRun.summary);
      const criteria = current.acceptanceCriteria.filter((criterion) => criterion.method === 'review');
      if (!review) return { ok: false, summary: 'Structured Issue-to-PR review was invalid; no evidence accepted.', evidence: criteria.map((criterion) => ({ evidenceId: `issue-review-${params.runId}-${criterion.id}`, criterionId: criterion.id, name: 'Issue-to-PR structured review', passed: false, authoritative: false, stale: false, summary: reviewRun.summary, exitCode: 1, durationMs: 0, createdAtMs: Date.now(), source: 'review' as const })) };
      const passed = review.verdict === 'pass' && !review.findings.some((finding) => finding.severity === 'blocking');
      return { ok: passed, summary: JSON.stringify(review), review, evidence: criteria.map((criterion) => ({ evidenceId: `issue-review-${params.runId}-${criterion.id}`, criterionId: criterion.id, name: 'Issue-to-PR structured review', passed, authoritative: true, stale: false, summary: JSON.stringify(review), exitCode: passed ? 0 : 1, durationMs: 0, createdAtMs: Date.now(), source: 'review' as const })) };
    },
  };
  const started = await startAutonomousTask({ objective: `Implement GitHub issue #${params.issueNumber}: ${params.issueTitle}`, sessionId: params.sessionId ?? null, constraints: { strategy: 'DELEGATE', source: 'issue', untrustedSource: true, allowExternalDelivery: true }, deliveryIntent: 'open_or_update_pr', deliveryTarget, runtime, signal: params.signal });
  const result = await started.completion;
  const implementationResult = implementation as IssueToPrAgentResult | null;
  return { outcome: result.outcome === 'SUCCEEDED' ? 'completed' : result.outcome === 'CANCELLED' ? 'cancelled' : 'error', summary: result.summary ?? 'Issue-to-PR task did not complete.', durableRunId: implementationResult?.durableRunId ?? null, task: result };
}

function buildSystemPrompt(params: RunIssueToPrAgentParams): string {
  return [
    'You are Little Monkey, running the Issue-to-PR Agent Flow: a headless, panel-driven run with no one watching live — never ask a question, just make the best reasonable call and note any assumption in your final summary.',
    `Your task is issue #${params.issueNumber} in ${params.repositorySlug}, already checked out on the app-owned branch "${params.branch}".`,
    `Every file, list_dir, glob, grep, write_file, edit_file, and run_shell path/cwd you use MUST be prefixed with "${params.workspaceLabel}/" — that is the only root this run may touch. Never use an absolute path or an unprefixed relative path.`,
    'Read the relevant code first, then make the minimal correct change for the issue. Prefer small, reviewable diffs over a broad rewrite.',
    "Once the change looks complete, detect and run this repository's own test/build scripts yourself (e.g. read package.json for a \"test\"/\"build\" script and run it with run_shell) and fix anything they surface before finishing.",
    'Hard limits, never do any of these — they stay outside this flow entirely and are handled by a human reviewer afterward: do not run `git merge`, do not force-push, do not delete any branch, and do not attempt to resolve or reply to a GitHub PR review thread.',
    'When you are done, reply with a short final summary: what you changed and why, and the result of the checks you ran. Do not call any more tools after that summary.',
  ].join('\n');
}

/**
 * Runs the model->tools->model loop to completion (a final assistant reply
 * with no further tool calls), the iteration cap, cancellation via `signal`,
 * or an unrecoverable stream error — whichever comes first. Never throws;
 * every outcome is reported through the returned `IssueToPrAgentResult`.
 */
export async function runIssueToPrAgent(
  params: RunIssueToPrAgentParams,
): Promise<IssueToPrAgentResult> {
  const systemPrompt = buildSystemPrompt(params);

  // The issue's title/body came from a GitHub API fetch, not the user typing
  // in this app — on a public repo anyone can open an issue — so it goes
  // through the same untrusted-content boundary every other external/
  // model-adjacent input reaches the model through (see `protectToolResult`,
  // `mentions.ts`, `crewRunner.ts`). The instruction to implement the issue
  // is the trusted part and stays outside the wrapped block.
  const issueContent = [`Title: ${params.issueTitle}`, '', params.issueBody.trim() || '(no description provided)'].join(
    '\n',
  );
  const userMessage = [
    `Implement GitHub issue #${params.issueNumber} in ${params.repositorySlug}. The issue's own title and body follow, fetched verbatim from GitHub:`,
    '',
    wrapUntrustedContent(`GitHub issue #${params.issueNumber} (${params.repositorySlug})`, issueContent),
  ].join('\n');

  return runHeadlessAgent({
    runId: params.runId,
    signal: params.signal,
    systemPrompt,
    userMessage,
    maxIterations: MAX_ISSUE_TO_PR_ITERATIONS,
    executionSource: 'issue-to-pr',
    durableRun: {
      task: `Issue-to-PR #${params.issueNumber}: ${params.issueTitle}`,
      instructions: `Owned branch ${params.branch} in ${params.repositorySlug}`,
    },
    onToolActivity: params.onToolActivity,
  });
}
