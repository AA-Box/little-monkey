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
import { effortForTarget } from '../store/modelStore';
import { useWorkspaceStore } from '../store/workspaceStore';
import { usePermissionStore } from '../store/permissionStore';
import { resolveTarget, snapshotForResolvedTarget } from './agentLoop';
import { beginDurableRun, type DurableRunRecorder } from './durableRun';
import type { ChatMessage } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { toolsForProfile } from './tools';
import {
  attemptStream,
  executeToolCall,
  isToolCallAllowed,
  stringifyToolError,
  CANCELLED_TOOL_RESULT,
} from './turnEngine';
import { protectToolResult } from './untrustedContent';

/** Hard cap on model/tool round trips — generous relative to
 * `subagent.ts`'s `MAX_SUBAGENT_ITERATIONS` (15) since a full issue
 * implementation (read around, edit, run tests, fix, re-run) is a much
 * larger task than a single delegated subtask. */
export const MAX_ISSUE_TO_PR_ITERATIONS = 40;

function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

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
  const { runId, signal } = params;
  const target = await resolveTarget();
  const effort = effortForTarget(target);
  const tools = toolsForProfile('code');
  const mcpRegistry = emptyMcpRegistry();
  const systemPrompt = buildSystemPrompt(params);

  const userMessage = [
    `Issue #${params.issueNumber} — ${params.issueTitle}`,
    '',
    params.issueBody.trim() || '(no description provided)',
  ].join('\n');

  let messages: ChatMessage[] = [{ role: 'user', content: userMessage }];

  const targetSnapshot = snapshotForResolvedTarget(target);
  const recorder: DurableRunRecorder | null = targetSnapshot
    ? await beginDurableRun({
        runId,
        kind: 'background',
        task: `Issue-to-PR #${params.issueNumber}: ${params.issueTitle}`,
        instructions: `Owned branch ${params.branch} in ${params.repositorySlug}`,
        target: targetSnapshot,
        roots: useWorkspaceStore.getState().roots,
        permissionMode: usePermissionStore.getState().mode,
        allowNetwork: false,
        allowExternalMutations: false,
      }).catch(() => null)
    : null;

  const finish = async (
    outcome: IssueToPrAgentResult['outcome'],
    summary: string,
  ): Promise<IssueToPrAgentResult> => {
    if (recorder) {
      if (outcome === 'completed') await recorder.complete(summary).catch(() => {});
      else if (outcome === 'cancelled') await recorder.cancel(summary).catch(() => {});
      else await recorder.fail(summary).catch(() => {});
    }
    return { outcome, summary, durableRunId: recorder?.runId ?? null };
  };

  try {
    for (let iteration = 0; iteration < MAX_ISSUE_TO_PR_ITERATIONS; iteration++) {
      if (signal.aborted) return finish('cancelled', 'Cancelled by the user.');

      const wireHistory: ChatMessage[] = [{ role: 'system', content: systemPrompt }, ...messages];
      const attempt = await attemptStream(target, wireHistory, tools, signal, effort, runId);

      if (attempt.usage) {
        recorder?.recordUsage(attempt.usage.promptTokens, attempt.usage.completionTokens);
      }
      if (attempt.streamError !== null) return finish('error', attempt.streamError);

      if (attempt.toolCalls.length === 0) {
        const finalMessage: ChatMessage = { role: 'assistant', content: attempt.content };
        messages = [...messages, finalMessage];
        if (attempt.content) recorder?.recordModelOutput(`${runId}:${iteration}`, attempt.content);
        return finish('completed', attempt.content.trim() || 'Agent finished with no summary.');
      }

      const assistantMessage: ChatMessage = {
        role: 'assistant',
        content: attempt.content,
        tool_calls: attempt.toolCalls,
      };
      messages = [...messages, assistantMessage];
      if (attempt.content) recorder?.recordModelOutput(`${runId}:${iteration}`, attempt.content);

      for (const toolCall of attempt.toolCalls) {
        const aborted = signal.aborted;
        const allowed = isToolCallAllowed(toolCall, tools);
        if (!aborted) {
          params.onToolActivity?.(toolCall.function.name);
          await recorder
            ?.recordToolProposed(toolCall.id, toolCall.function.name, toolCall.function.arguments ?? '')
            .catch(() => {});
          recorder?.recordToolStarted(toolCall.id);
        }
        const started = Date.now();
        const resultContent = aborted
          ? CANCELLED_TOOL_RESULT
          : !allowed
            ? stringifyToolError(
                new Error(`Tool "${toolCall.function.name}" was not offered to this run.`),
              )
            : await executeToolCall(
                toolCall,
                null,
                runId,
                mcpRegistry,
                signal,
                undefined,
                undefined,
                undefined,
                'issue-to-pr',
              );
        if (!aborted) {
          await recorder?.recordToolFinished(toolCall.id, resultContent, Date.now() - started).catch(() => {});
        }
        const toolMessage: ChatMessage = {
          role: 'tool',
          tool_call_id: toolCall.id,
          content: allowed ? protectToolResult(toolCall.function.name, resultContent, false) : resultContent,
        };
        messages = [...messages, toolMessage];
      }

      if (signal.aborted) return finish('cancelled', 'Cancelled by the user.');
    }

    return finish(
      'error',
      `Stopped after reaching the safety limit of ${MAX_ISSUE_TO_PR_ITERATIONS} tool-calling iterations without a final answer.`,
    );
  } catch (err) {
    return finish('error', err instanceof Error ? err.message : String(err));
  }
}
