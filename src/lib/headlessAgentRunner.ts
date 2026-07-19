/**
 * Shared model -> tools -> model loop for panel-driven background agents.
 *
 * Feature modules own their domain prompts and durable-run labels. This
 * module owns the safety-critical mechanics so permission checks,
 * cancellation, result protection, evidence recording, and loop limits do
 * not drift between Issue-to-PR, migration slices, and security autofix.
 */
import { effortForTarget } from '../store/modelStore';
import { usePermissionStore } from '../store/permissionStore';
import { useWorkspaceStore } from '../store/workspaceStore';
import { resolveTarget, snapshotForResolvedTarget } from './agentLoop';
import { beginDurableRun, type DurableRunRecorder } from './durableRun';
import type { ChatMessage, ToolCall } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { toolsForProfile } from './tools';
import {
  attemptStream,
  CANCELLED_TOOL_RESULT,
  executeToolCall,
  isToolCallAllowed,
  stringifyToolError,
} from './turnEngine';
import { protectToolResult } from './untrustedContent';
import { isVisionCapableProviderModel } from './visionModels';

const CANCELLED_SUMMARY = 'Cancelled by the user.';

function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

export interface HeadlessAgentResult {
  outcome: 'completed' | 'cancelled' | 'error';
  summary: string;
  durableRunId: string | null;
}

export interface HeadlessAgentDurableRunSpec {
  task: string;
  instructions: string | null;
}

export interface RunHeadlessAgentParams {
  runId: string;
  signal: AbortSignal;
  systemPrompt: string;
  userMessage: string;
  /** Optional OpenAI-style multipart user content. This is intentionally
   * limited to the first user turn so feature runners can attach local
   * screenshots without inventing a second model transport. `userMessage`
   * remains required as the text/audit fallback and for existing callers. */
  userContent?: ChatMessage['content'];
  /** Fail before a model request when the selected target cannot inspect
   * images. This prevents screenshot-driven workflows from silently asking a
   * text-only model to guess at pixels. */
  requireVision?: boolean;
  maxIterations: number;
  /** Defaults to `code`; use `explore` for analysis-only agents so mutating
   * tools are absent from both the model schema and durable workspace policy. */
  toolProfile?: 'explore' | 'code';
  /** Attribution forwarded to permission prompts for mutating tool calls. */
  executionSource: string;
  /** When set, every workspace-scoped tool must explicitly target this
   * attached-root label. This keeps owned-worktree agents from accidentally
   * reading or mutating the primary root when a model omits its path/cwd. */
  requiredWorkspaceRoot?: string;
  durableRun: HeadlessAgentDurableRunSpec;
  onToolActivity?: (label: string) => void;
  /** Optional feature-level validation of the final reply. Throwing converts
   * the run to an error before its durable capsule is marked complete. */
  validateFinal?: (summary: string) => void | Promise<void>;
}

function workspaceRootViolation(toolCall: ToolCall, requiredRoot: string | undefined): string | null {
  if (!requiredRoot) return null;
  let args: Record<string, unknown>;
  try {
    const parsed = JSON.parse(toolCall.function.arguments || '{}') as unknown;
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return 'Tool arguments must be a JSON object.';
    }
    args = parsed as Record<string, unknown>;
  } catch {
    return 'Tool arguments must be valid JSON before their workspace root can be verified.';
  }

  const field = toolCall.function.name === 'run_shell' ? 'cwd' : 'path';
  const rawPath = args[field];
  if (typeof rawPath !== 'string' || !rawPath.trim()) {
    return `Tool "${toolCall.function.name}" must provide ${field} inside attached root "${requiredRoot}".`;
  }
  const normalizedPath = rawPath.trim().replace(/\\/g, '/').replace(/^\.\//, '').replace(/\/+$/, '');
  const normalizedRoot = requiredRoot.trim().replace(/\\/g, '/').replace(/^\.\//, '').replace(/\/+$/, '');
  if (normalizedPath !== normalizedRoot && !normalizedPath.startsWith(`${normalizedRoot}/`)) {
    return `Tool "${toolCall.function.name}" may only target attached root "${requiredRoot}".`;
  }
  return null;
}

/**
 * Runs a background agent with the requested tool profile until it returns a final answer,
 * fails, is cancelled, or reaches its configured iteration cap. All
 * failures are returned as a typed result; callers never need a second error
 * path alongside the normal outcome handling.
 */
export async function runHeadlessAgent(params: RunHeadlessAgentParams): Promise<HeadlessAgentResult> {
  const { runId, signal } = params;
  let recorder: DurableRunRecorder | null = null;

  const finish = async (
    outcome: HeadlessAgentResult['outcome'],
    summary: string,
  ): Promise<HeadlessAgentResult> => {
    if (recorder) {
      if (outcome === 'completed') await recorder.complete(summary).catch(() => {});
      else if (outcome === 'cancelled') await recorder.cancel(summary).catch(() => {});
      else await recorder.fail(summary).catch(() => {});
    }
    return { outcome, summary, durableRunId: recorder?.runId ?? null };
  };

  try {
    const target = await resolveTarget();
    const effort = effortForTarget(target);
    const toolProfile = params.toolProfile ?? 'code';
    const tools = toolsForProfile(toolProfile);
    const mcpRegistry = emptyMcpRegistry();
    let messages: ChatMessage[] = [{ role: 'user', content: params.userContent ?? params.userMessage }];

    const targetSnapshot = snapshotForResolvedTarget(target);
    const visionCapable = target.kind === 'provider'
      ? isVisionCapableProviderModel(target.providerId, target.model)
      : targetSnapshot?.capabilities?.vision?.state === 'yes';
    if (params.requireVision && !visionCapable) {
      return finish(
        'error',
        'This run includes image sources, but the selected model is not configured as vision-capable. Select a vision model (or correct its Vision override) and retry.',
      );
    }
    recorder = targetSnapshot
      ? await beginDurableRun({
          runId,
          kind: 'background',
          task: params.durableRun.task,
          instructions: params.durableRun.instructions,
          target: targetSnapshot,
          roots: useWorkspaceStore.getState().roots,
          permissionMode: usePermissionStore.getState().mode,
          allowNetwork: false,
          allowExternalMutations: false,
          workspaceAccess: toolProfile === 'explore' ? 'read_only' : 'read_write',
        }).catch(() => null)
      : null;

    for (let iteration = 0; iteration < params.maxIterations; iteration++) {
      if (signal.aborted) return finish('cancelled', CANCELLED_SUMMARY);

      const wireHistory: ChatMessage[] = [
        { role: 'system', content: params.systemPrompt },
        ...messages,
      ];
      const attempt = await attemptStream(target, wireHistory, tools, signal, effort, runId);

      if (attempt.usage) {
        recorder?.recordUsage(attempt.usage.promptTokens, attempt.usage.completionTokens);
      }
      if (attempt.streamError !== null) return finish('error', attempt.streamError);

      if (attempt.toolCalls.length === 0) {
        if (attempt.content) recorder?.recordModelOutput(`${runId}:${iteration}`, attempt.content);
        const summary = attempt.content.trim() || 'Agent finished with no summary.';
        await params.validateFinal?.(summary);
        return finish('completed', summary);
      }

      messages = [
        ...messages,
        { role: 'assistant', content: attempt.content, tool_calls: attempt.toolCalls },
      ];
      if (attempt.content) recorder?.recordModelOutput(`${runId}:${iteration}`, attempt.content);

      for (const toolCall of attempt.toolCalls) {
        const aborted = signal.aborted;
        const offered = isToolCallAllowed(toolCall, tools);
        const rootViolation = offered
          ? workspaceRootViolation(toolCall, params.requiredWorkspaceRoot)
          : null;
        const allowed = offered && !rootViolation;
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
                new Error(rootViolation ?? `Tool "${toolCall.function.name}" was not offered to this run.`),
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
                params.executionSource,
              );

        if (!aborted) {
          await recorder?.recordToolFinished(toolCall.id, resultContent, Date.now() - started).catch(() => {});
        }
        messages = [
          ...messages,
          {
            role: 'tool',
            tool_call_id: toolCall.id,
            content: allowed
              ? protectToolResult(toolCall.function.name, resultContent, false)
              : resultContent,
          },
        ];
      }

      if (signal.aborted) return finish('cancelled', CANCELLED_SUMMARY);
    }

    return finish(
      'error',
      `Stopped after reaching the safety limit of ${params.maxIterations} tool-calling iterations without a final answer.`,
    );
  } catch (error) {
    return finish('error', error instanceof Error ? error.message : String(error));
  }
}
