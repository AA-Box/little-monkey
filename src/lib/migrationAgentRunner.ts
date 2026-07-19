/**
 * The Migration and Upgrade Agent's headless "implement slice #1" phase —
 * drives a REAL agent turn (not a scripted templated diff) against an owned
 * worktree `migrationAgentStore.ts` already created via `gitDelivery.ts`,
 * using the EXACT same primitives `issueToPrRunner.ts` uses for its own
 * headless "implement the issue" phase: `turnEngine.ts`'s
 * `attemptStream`/`executeToolCall`, `tools.ts`'s `toolsForProfile('code')`,
 * and the same Run Capsule ledger every other run writes to
 * (`durableRun.ts`'s `beginDurableRun`, `kind: 'background'`).
 *
 * MVP scope: only ONE slice of a `MigrationPlan` (`migrationAgent.ts`) is
 * ever turned into a real code change this way — always slice #1, and only
 * after the user has approved attempting it (see `migrationAgentStore.ts`'s
 * `attemptFirstSlice`). The remaining slices stay a plan-only follow-up
 * checklist; nothing here executes them.
 *
 * Structurally this is `issueToPrRunner.ts`'s `runIssueToPrAgent` adapted for
 * a migration slice instead of a GitHub issue: same model->tools->model loop
 * shape, same permission-gated tool dispatch (write_file/edit_file/run_shell
 * still prompt exactly like any other agent-initiated mutation — nothing
 * here bypasses that), reporting progress to `migrationAgentStore.ts` instead
 * of `issueToPrStore.ts`.
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
import { buildModelTargetInventory, type ModelTargetSnapshot } from './modelTargets';
import { resolveTarget } from './agentLoop';
import { beginDurableRun, type DurableRunRecorder } from './durableRun';
import type { ChatMessage } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { useModelStore } from '../store/modelStore';
import { toolsForProfile } from './tools';
import {
  attemptStream,
  executeToolCall,
  isToolCallAllowed,
  stringifyToolError,
  CANCELLED_TOOL_RESULT,
  type ResolvedTarget,
} from './turnEngine';
import { protectToolResult } from './untrustedContent';
import type { MigrationSlice } from './migrationAgent';

/** Hard cap on model/tool round trips for one slice — same order of
 * magnitude as `issueToPrRunner.ts`'s `MAX_ISSUE_TO_PR_ITERATIONS` (a full
 * slice implementation is a comparably sized task: read around, edit, run
 * tests, fix, re-run). */
export const MAX_MIGRATION_SLICE_ITERATIONS = 40;

function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

/**
 * Resolves a streaming target back to the immutable inventory record that
 * contains its endpoint/model/capability evidence, for the Run Capsule
 * ledger. Deliberately reimplemented here from the public `./modelTargets`
 * building block rather than imported from `agentLoop.ts` (whose own copy is
 * module-private) — see `issueToPrRunner.ts` for the sibling flow that
 * reused an exported copy of the same helper before this one existed.
 */
function snapshotForResolvedTarget(target: ResolvedTarget): ModelTargetSnapshot | null {
  const state = useModelStore.getState();
  const inventory = buildModelTargetInventory({
    installed: state.installed,
    active: state.active,
    llamaStatus: state.llamaStatus,
    ollamaModels: state.ollamaModels,
    ollamaReachable: state.ollamaReachable,
    providers: state.providers,
    providerModels: state.providerModels,
    effortByTarget: state.effortByTarget,
  });
  if (target.kind === 'local') {
    return inventory.targets.find((candidate) => candidate.kind === 'local') ?? null;
  }
  if (target.kind === 'ollama') {
    return inventory.targets.find(
      (candidate) => candidate.kind === 'ollama' && candidate.model === target.model,
    ) ?? null;
  }
  return inventory.targets.find(
    (candidate) =>
      candidate.kind === 'provider' &&
      candidate.providerId === target.providerId &&
      candidate.model === target.model,
  ) ?? null;
}

export interface RunMigrationSliceParams {
  /** Reused as both the headless loop's own `turnId` (scoping Rust-side
   * permission prompts/cancellation) and the Run Capsule ledger's `run_id` —
   * one id, one evidence trail, exactly one migration run. */
  runId: string;
  goal: string;
  slice: MigrationSlice;
  branch: string;
  /** The secondary workspace root label `migrationAgentStore.ts` attached
   * for this run's owned worktree — every tool path the model uses must be
   * prefixed with `"<label>/"` to land inside it. */
  workspaceLabel: string;
  signal: AbortSignal;
  /** Called once per tool call the model makes, purely for the panel's live
   * "current activity" line — never gates anything. */
  onToolActivity?: (label: string) => void;
}

export interface MigrationSliceAgentResult {
  outcome: 'completed' | 'cancelled' | 'error';
  /** The agent's own final summary (or an error/cancellation message). */
  summary: string;
  /** The Run Capsule ledger id this run was recorded under, if the desktop
   * host's run-protocol version matched (see `beginDurableRun`'s doc
   * comment) — `null` on an older host, where the flow still runs but has
   * no capsule to show. */
  durableRunId: string | null;
}

function buildSystemPrompt(params: RunMigrationSliceParams): string {
  return [
    'You are Little Monkey, running the Migration and Upgrade Agent: a headless, panel-driven run implementing ONE slice of a larger migration/upgrade plan — never ask a question, just make the best reasonable call and note any assumption in your final summary.',
    `Overall migration goal: ${params.goal}`,
    `Your task is slice ${params.slice.order} of the plan, "${params.slice.title}": ${params.slice.description || '(no further description provided)'}`,
    `Known risk notes for this slice: ${params.slice.riskNotes.length > 0 ? params.slice.riskNotes.join('; ') : 'none noted'}.`,
    `Rollback plan if this slice goes wrong: ${params.slice.rollbackNotes}`,
    `This is already checked out on the app-owned branch "${params.branch}".`,
    `Every file, list_dir, glob, grep, write_file, edit_file, and run_shell path/cwd you use MUST be prefixed with "${params.workspaceLabel}/" — that is the only root this run may touch. Never use an absolute path or an unprefixed relative path.`,
    'Read the relevant code first, then make the minimal correct change for THIS SLICE ONLY. Do not attempt any later slice of the plan, even if you notice it while working.',
    "Once the change looks complete, detect and run this repository's own test/build scripts yourself (e.g. read package.json for a \"test\"/\"build\" script, or check for a Rust crate via Cargo.toml, and run it with run_shell) and fix anything they surface before finishing.",
    'Hard limits, never do any of these — they stay outside this flow entirely and are handled by a human reviewer afterward: do not run `git merge`, do not force-push, do not delete any branch, and do not open or modify a pull request yourself.',
    'When you are done, reply with a short final summary: what you changed and why, and the result of the checks you ran (pass/fail, and any remaining risk for a human reviewer to watch for). Do not call any more tools after that summary.',
  ].join('\n');
}

/**
 * Runs the model->tools->model loop to completion (a final assistant reply
 * with no further tool calls), the iteration cap, cancellation via `signal`,
 * or an unrecoverable stream error — whichever comes first. Never throws;
 * every outcome is reported through the returned `MigrationSliceAgentResult`.
 */
export async function runMigrationSliceAgent(
  params: RunMigrationSliceParams,
): Promise<MigrationSliceAgentResult> {
  const { runId, signal } = params;
  const target = await resolveTarget();
  const effort = effortForTarget(target);
  const tools = toolsForProfile('code');
  const mcpRegistry = emptyMcpRegistry();
  const systemPrompt = buildSystemPrompt(params);

  const userMessage = [
    `Implement slice ${params.slice.order} ("${params.slice.title}") of the migration plan for: ${params.goal}`,
    '',
    params.slice.description || '(no further description provided)',
  ].join('\n');

  let messages: ChatMessage[] = [{ role: 'user', content: userMessage }];

  const targetSnapshot = snapshotForResolvedTarget(target);
  const recorder: DurableRunRecorder | null = targetSnapshot
    ? await beginDurableRun({
        runId,
        kind: 'background',
        task: `Migration slice ${params.slice.order}: ${params.slice.title}`,
        instructions: `Owned branch ${params.branch}; goal: ${params.goal}`,
        target: targetSnapshot,
        roots: useWorkspaceStore.getState().roots,
        permissionMode: usePermissionStore.getState().mode,
        allowNetwork: false,
        allowExternalMutations: false,
      }).catch(() => null)
    : null;

  const finish = async (
    outcome: MigrationSliceAgentResult['outcome'],
    summary: string,
  ): Promise<MigrationSliceAgentResult> => {
    if (recorder) {
      if (outcome === 'completed') await recorder.complete(summary).catch(() => {});
      else if (outcome === 'cancelled') await recorder.cancel(summary).catch(() => {});
      else await recorder.fail(summary).catch(() => {});
    }
    return { outcome, summary, durableRunId: recorder?.runId ?? null };
  };

  try {
    for (let iteration = 0; iteration < MAX_MIGRATION_SLICE_ITERATIONS; iteration++) {
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
                'migration-agent',
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
      `Stopped after reaching the safety limit of ${MAX_MIGRATION_SLICE_ITERATIONS} tool-calling iterations without a final answer.`,
    );
  } catch (err) {
    return finish('error', err instanceof Error ? err.message : String(err));
  }
}
