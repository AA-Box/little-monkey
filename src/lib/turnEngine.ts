/**
 * The core turn-execution engine: one streaming model attempt
 * (`attemptStream`) and one tool-call dispatch (`executeToolCall`), plus the
 * private helpers only those two functions use.
 *
 * Extracted out of `agentLoop.ts` verbatim (no behavior change) so future
 * consumers beyond the top-level chat loop — subagents, the verify loop,
 * Plan/Act mode — can drive the same primitives without depending on
 * `agentLoop.ts`'s orchestration layer (turn lifecycle, checkpoints,
 * failover/vision-switch selection, context compaction). `agentLoop.ts`
 * still owns all of that orchestration and calls into this module for the
 * two loop-exit-adjacent primitives themselves.
 */
import { invoke } from '@tauri-apps/api/core';
import { streamChat } from './llamaClient';
import type { ChatMessage, StreamEvent, ToolCall, ToolDef } from './llamaClient';
import { streamProviderChat } from './providerClient';
import { formatMcpCallToolResult, resolveMcpToolName, type McpCallToolResult, type McpToolRegistry } from './mcpTools';
import { recordRequest } from './rateLimitTracker';
import { useUsageStore } from '../store/usageStore';
import { riskCacheKey, type RiskClassification } from './riskJudge';
import { runSubagentTask } from './subagent';

/** Where a turn's requests should go. Local llama.cpp and Ollama are kept
 * distinct (rather than a single generic "direct fetch" kind) so
 * failover/vision-switch logic can tell exactly which store setter
 * (`useOllamaModel` vs `useProviderModel`) to call when it picks a
 * different target — both still stream via the same `streamChat` transport. */
export type ResolvedTarget =
  | { kind: 'local'; baseUrl: string }
  | { kind: 'ollama'; baseUrl: string; model: string }
  | { kind: 'provider'; providerId: string; model: string };

/** Stringifies a tool invocation's result (or error) for use as tool-message content. */
function stringifyToolResult(result: unknown): string {
  if (typeof result === 'string') return result;
  try {
    return JSON.stringify(result);
  } catch {
    return String(result);
  }
}

export function stringifyToolError(err: unknown): string {
  const message = err instanceof Error ? err.message : typeof err === 'string' ? err : JSON.stringify(err);
  return JSON.stringify({ error: message });
}

/** The tool-message content used for a call the user's Stop button cancelled
 * (either mid-execution, or before it ever started). A result message is
 * still recorded for every requested call so the persisted transcript never
 * contains an assistant `tool_calls` entry without its matching results —
 * several providers reject such a history outright on the next turn. */
export const CANCELLED_TOOL_RESULT = JSON.stringify({ error: 'Cancelled by the user' });

/**
 * The tool-message content returned for a `present_plan` call — see
 * `tools.ts`'s `PRESENT_PLAN_TOOL` doc comment for why this tool has no
 * `tool_present_plan` Rust command. Deliberately a fixed literal, not
 * anything derived from the model's own arguments: the result content only
 * needs to end the model's turn cleanly, not carry data back to it — the
 * plan's actual title/body/open-questions are surfaced to the *user* as a
 * `PlanNotice` transcript notice, appended by `agentLoop.ts`'s tool-calling
 * loop from this same call's arguments (this function only returns the tool
 * result string; the transcript side effect lives one layer up, same split
 * responsibility as the `remember` tool's result vs. its `MemoryNotice`).
 */
export const PRESENT_PLAN_RESULT = JSON.stringify({
  status: 'plan_presented',
  note: 'Wait for the user to approve before doing anything else.',
});

/** Resolves when `signal` aborts (never resolves for an undefined signal).
 * Exported so other callers that race a Tauri `invoke` against Stop —
 * currently just `runVerificationPhase` in `agentLoop.ts` — can reuse the
 * exact same race/cancel shape `executeToolCall` uses below instead of
 * hand-rolling their own. */
export function abortedPromise(signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    signal.addEventListener('abort', () => resolve(), { once: true });
  });
}

/**
 * Dispatches a `mcp__<serverId>__<toolName>`-named tool call to the Rust
 * `mcp_call_tool` command. `serverId`/`toolName` are resolved via
 * `resolveMcpToolName` against `mcpRegistry` — THIS turn's own
 * `mcpToolDefs()` result, passed in by the caller rather than read from any
 * shared/module-level state — rather than re-parsed out of `name` itself;
 * see `mcpTools.ts`'s doc comment for why a naive split on `__` isn't
 * reliably reversible, and for why the registry must be turn-scoped rather
 * than a shared singleton (a concurrent split-pane turn's own
 * `mcpToolDefs()` call must never be able to invalidate or repoint a name
 * THIS turn's model was already offered).
 *
 * No `checkpoint_id` is injected here (unlike write_file/edit_file/run_shell
 * below): MCP side effects are explicitly outside the checkpoint revert
 * guarantee, same documented gap as `run_shell`'s shell commands (see
 * `CheckpointNotice.shellRan`'s doc comment). `turn_id` still is, though —
 * it scopes this call's permission prompt and Stop-button cancellation to
 * this turn, via the same `AppState.tool_cancel` mechanism `run_shell` uses.
 */
function invokeMcpTool(
  name: string,
  args: Record<string, unknown>,
  turnId: string,
  mcpRegistry: McpToolRegistry
): Promise<string> {
  const resolved = resolveMcpToolName(mcpRegistry, name);
  if (!resolved) {
    return Promise.resolve(stringifyToolError(new Error(`MCP tool "${name}" was not offered this turn.`)));
  }
  return invoke<McpCallToolResult>('mcp_call_tool', {
    server_id: resolved.serverId,
    tool_name: resolved.toolName,
    arguments: args,
    turn_id: turnId,
  }).then(formatMcpCallToolResult, stringifyToolError);
}

/** Tool names eligible for risk classification — see `RiskAnnotationContext`.
 * `run_shell` is included for DISPLAY purposes only (the permission modal can
 * show a badge on a shell prompt too) — see `permissions.rs`'s
 * `tool_run_shell` doc comment for the load-bearing invariant this must never
 * violate: nothing computed here ever feeds into whether `run_shell` (or
 * anything else) gets auto-approved. */
const RISK_ELIGIBLE_TOOLS = new Set(['write_file', 'edit_file', 'run_shell']);

/**
 * Everything `executeToolCall` needs to attach an advisory risk annotation to
 * a mutating tool call — see `riskJudge.ts`'s module doc comment for why
 * `classify` is a plain injected callback rather than this module importing
 * `classifyToolCall` directly (it would create an import cycle). Built once
 * per turn by `agentLoop.ts`'s `runAgentTurnBody` (`cache` in particular must
 * survive across this turn's tool-calling round trips, exactly like
 * `mutatedFiles`) and passed down unchanged.
 */
export interface RiskAnnotationContext {
  /** Mirrors `settingsStore`'s `riskAnnotationsEnabled` — when `false`,
   * classification is skipped entirely (model-supplied risk keys are still
   * scrubbed either way, see below) and no risk args are injected. */
  enabled: boolean;
  /** Keyed by `riskJudge.ts`'s `riskCacheKey(tool, args)` — reused across
   * this turn's round trips so a repeated identical call never pays for (or
   * waits on) a second judge round trip. */
  cache: Map<string, RiskClassification | null>;
  /** Runs the actual one-shot judge call — an `agentLoop.ts` closure around
   * `attemptStream` and the turn's current target (see `riskJudge.ts`'s
   * `classifyToolCall`, which this wraps). */
  classify: (tool: string, args: Record<string, unknown>) => Promise<RiskClassification | null>;
}

/**
 * Everything `executeToolCall` needs to delegate a `task` tool call to
 * `subagent.ts`'s `runSubagentTask` — built once per turn by
 * `agentLoop.ts`'s `runAgentTurnBody` (mirrors `RiskAnnotationContext`'s
 * "built once, passed down unchanged" shape) and passed through every
 * `executeToolCall` call this turn, exactly like `risk`/`attachedStackNames`.
 * Omitted entirely by callers that never offer the `task` tool (e.g. today's
 * only other caller besides the main loop, if any existed) — a `task` call
 * reaching `executeToolCall` with no context configured is defensively
 * reported as a tool error rather than throwing.
 */
export interface SubagentContext {
  /** Threaded through to `runSubagentTask` for `useUsageStore` keying — see
   * that function's own doc comment for why child usage is never actually
   * recorded under it (the whole point of `recordUsage: false`). */
  sessionId: string;
  /** THIS turn's already-resolved active target (see `ResolvedTarget`) —
   * passed down rather than re-resolved, so a mid-turn manual model switch
   * can never split the parent and child across different targets. */
  target: ResolvedTarget;
  effort?: string;
}

/**
 * Executes a single model-requested tool call via the corresponding
 * `tool_<name>` Tauri command (or, for an `mcp__`-namespaced name, via
 * `invokeMcpTool` above) and returns the string to use as the content of the
 * resulting `tool` message. Never throws — invocation errors (bad JSON
 * arguments, permission denial, sandbox violations, command failures) are
 * captured and returned as a JSON error payload so the model can see what
 * went wrong and try to recover instead of the whole loop crashing.
 *
 * If `signal` aborts while the command is in flight, the Rust side is told
 * to cancel everything cancellable (`tools_cancel_running` kills any running
 * shell child and denies any pending permission prompt) and a cancelled
 * result is returned immediately rather than waiting the command out.
 */
export async function executeToolCall(
  toolCall: ToolCall,
  checkpointId: string | null,
  turnId: string,
  mcpRegistry: McpToolRegistry,
  signal?: AbortSignal,
  risk?: RiskAnnotationContext,
  attachedStackNames?: string[],
  subagent?: SubagentContext
): Promise<string> {
  const { name, arguments: rawArguments } = toolCall.function;

  let args: Record<string, unknown> = {};
  if (rawArguments && rawArguments.trim().length > 0) {
    try {
      const parsed: unknown = JSON.parse(rawArguments);
      if (parsed && typeof parsed === 'object') {
        args = parsed as Record<string, unknown>;
      }
    } catch (err) {
      return stringifyToolError(new Error(`Invalid tool call arguments JSON for "${name}": ${(err as Error).message}`));
    }
  }

  // `risk_level`/`risk_reason` are frontend-owned, exactly like
  // `checkpoint_id`/`turn_id` below — the model must never be able to smuggle
  // its own risk rating through its tool-call arguments (a model that always
  // claimed "low" for its own edits would defeat the entire point of an
  // independent judge). Unconditionally deleted here, BEFORE anything else
  // touches `args`, regardless of tool name or whether risk annotations are
  // even enabled this turn, so there is no code path — now or from a future
  // change to `RISK_ELIGIBLE_TOOLS` — where a model-supplied value survives.
  delete args.risk_level;
  delete args.risk_reason;

  // Classify (cached per turn) BEFORE the checkpoint_id/turn_id injection
  // below, so both the cache key and the judge prompt reflect only the
  // model's actual call — not internal bookkeeping fields it never provided.
  if (RISK_ELIGIBLE_TOOLS.has(name) && risk?.enabled) {
    const key = riskCacheKey(name, args);
    let classification: RiskClassification | null;
    if (risk.cache.has(key)) {
      classification = risk.cache.get(key) ?? null;
    } else {
      classification = await risk.classify(name, args);
      risk.cache.set(key, classification);
    }
    if (classification) {
      args.risk_level = classification.level;
      args.risk_reason = classification.reason;
    }
  }

  // File-mutating tools record a pre-mutation backup into this turn's own
  // checkpoint — with the split pane, another turn (with its own checkpoint)
  // may be running concurrently, so the id pins the backup to the right one.
  // run_shell doesn't snapshot anything, but gets the same injected id so
  // `record_shell` can flag the owning checkpoint's `shell_ran` — the
  // revert-coverage caveat the UI shows. Injected here rather than exposed in
  // the tool schema: the model must never pick (or fabricate) a checkpoint
  // id. snake_case key — write_file/edit_file/run_shell all use
  // `rename_all = "snake_case"` so the model's snake_case tool arguments
  // (old_string, new_string) match without translation.
  if (checkpointId !== null && (name === 'write_file' || name === 'edit_file' || name === 'run_shell')) {
    args.checkpoint_id = checkpointId;
  }
  // The turn id scopes permission prompts and shell/fetch cancellation to
  // THIS turn — Stop in one pane must not kill the other pane's command (or
  // in-flight fetch) or deny its prompt. Injected like checkpoint_id (never
  // model-supplied). All six commands use `rename_all = "snake_case"`, so
  // all take the snake_case key. `remember`/`web_fetch`/`web_search` don't
  // take a checkpoint_id (see tool_remember's/tool_web_fetch's/
  // tool_web_search's doc comments in tools.rs/web.rs — none snapshots a
  // workspace file), but all three are still permission-gated and need the
  // turn id for that prompt (and, for web_fetch only, for Stop-button
  // cancellation of the in-flight request — web_search's request is short
  // and fixed-endpoint, so it gets `remember`'s simpler "turn id for the
  // prompt only" treatment, see tool_web_search's doc comment).
  if (
    name === 'write_file' ||
    name === 'edit_file' ||
    name === 'run_shell' ||
    name === 'remember' ||
    name === 'web_fetch' ||
    name === 'web_search'
  ) {
    args.turn_id = turnId;
  }

  // `search_docs` is scoped to THIS session's actually-attached knowledge
  // stacks server-side, never left to the model to declare — same treatment
  // as `checkpoint_id`/`turn_id` above. Injected (and always overwritten,
  // even if the model's JSON args somehow already had a same-named key)
  // regardless of whether the model passed a `stack` argument: `stacks.rs`'s
  // `resolve_search_stack_ids` uses this as the allow-list for BOTH the
  // explicit-name case and the "omit stack" default-sweep case, so a
  // compliant model that just omits `stack` (the tool description's stated
  // default) can never sweep in a knowledge stack that exists and is indexed
  // but was never attached to this session — see that Rust function's doc
  // comment for the privacy gap this closes.
  if (name === 'search_docs') {
    args.allowed_stack_names = attachedStackNames ?? [];
  }

  // `present_plan` is a frontend-only tool (see `tools.ts`'s `PRESENT_PLAN_TOOL`
  // doc comment): it never reaches Rust at all, checked BEFORE the
  // mcp__/tool_<name> dispatch below so this holds regardless of what future
  // caller invokes `executeToolCall` for it, not just today's single call
  // site in `agentLoop.ts`.
  if (name === 'present_plan') {
    return PRESENT_PLAN_RESULT;
  }

  // `task` is another frontend-only tool, same treatment as `present_plan`
  // just above: it has no `tool_task` Rust command, so it's intercepted
  // here, before the `invoke`/`mcp__` dispatch below, and delegated to
  // `runSubagentTask` instead. This is the depth-cap-of-1 enforcement point
  // in practice (not just by the schema omission in `toolsForProfile`): even
  // if a future change somehow offered `task` to a subagent's own child
  // loop, `runSubagentTask` builds the child's tool list via
  // `toolsForProfile`, which never includes `task` — so there is no tool
  // name here for a grandchild call to even be named after.
  //
  // The whole branch is wrapped in try/catch (rather than trusting
  // `runSubagentTask`'s own internal one) so that ANY exception here —
  // including a bug in argument parsing below, not just inside the child's
  // own loop — can never propagate out of `executeToolCall` and leave this
  // call's `tool_calls` entry without a matching `tool` result (the
  // transcript-validity invariant every other branch in this function
  // upholds the same way).
  if (name === 'task') {
    try {
      if (!subagent) {
        return stringifyToolError(new Error('The task tool has no subagent execution context configured for this turn.'));
      }
      const description = typeof args.description === 'string' ? args.description : 'Subagent task';
      const taskPrompt = typeof args.prompt === 'string' ? args.prompt : '';
      // Only 'explore' is offered by `TASK_TOOL`'s schema this slice (see
      // `tools.ts`'s doc comment) — defensively re-validated here anyway,
      // rather than trusting the model's own JSON, exactly like every other
      // frontend-injected/validated field in this function.
      const profile: 'explore' | 'code' = args.profile === 'code' ? 'code' : 'explore';
      // The child's own turn id — NOT `turnId` (the parent's) — so its
      // tool calls get their own entry in the Rust per-turn `tool_cancel`/
      // permission maps (AppState, lib.rs), scoping Stop-button cancellation
      // and permission prompts to just this subagent run rather than
      // colliding with the parent turn's own. `checkpointId` (the parent's)
      // is passed through UNCHANGED below, so any file the child mutates
      // still lands in the parent turn's checkpoint manifest and is
      // revertable via the existing CheckpointRow — see `runSubagentTask`'s
      // doc comment for why this exact pairing (parent checkpoint id + own
      // turn id) is the crux of what makes subagents safe.
      const childTurnId = crypto.randomUUID();
      return await runSubagentTask({
        sessionId: subagent.sessionId,
        parentCheckpointId: checkpointId,
        parentSignal: signal,
        taskId: childTurnId,
        // The ORIGINATING `task` tool_call's own id — see
        // `RunSubagentTaskParams.toolCallId`'s doc comment for why this is a
        // deliberately separate id from `childTurnId` above: this is the
        // `subagentStore`/`ChatSession.subagentRuns` key `MessageList.tsx`
        // can actually correlate against the persisted transcript.
        toolCallId: toolCall.id,
        description,
        prompt: taskPrompt,
        profile,
        target: subagent.target,
        effort: subagent.effort,
      });
    } catch (err) {
      return stringifyToolError(err);
    }
  }

  const invocation = name.startsWith('mcp__')
    ? invokeMcpTool(name, args, turnId, mcpRegistry)
    : invoke(`tool_${name}`, args).then(stringifyToolResult, stringifyToolError);
  if (!signal) return invocation;

  const raced = await Promise.race([invocation, abortedPromise(signal).then(() => null)]);
  if (raced !== null) return raced;

  // Aborted mid-invocation: kill what can be killed on the Rust side. The
  // original invocation promise already has handlers attached (never an
  // unhandled rejection) and its eventual result is simply discarded.
  void invoke('tools_cancel_running', { turnId }).catch(() => {});
  return CANCELLED_TOOL_RESULT;
}

/** Result of a single streaming attempt against one target. */
interface AttemptResult {
  content: string;
  toolCalls: ToolCall[];
  streamError: string | null;
  /** Whether any content/tool-call fragment arrived before `streamError` (if any) — the failover safety rule below only ever retries a *different* target when this is `false`, since a mid-stream error has already shown the user partial output that a retry could duplicate or contradict. */
  contentStarted: boolean;
}

/**
 * Streams one chat-completion attempt against `target` and reports what
 * happened, without touching the session transcript itself — the caller
 * (`runAgentTurn`) owns writing content into the active session as it
 * streams in via `onDelta`, and owns deciding what a failure means (retry a
 * different target vs. surface the error).
 *
 * Every attempt through here — main turn or the one-shot summarization call
 * `contextTrimmer.ts` triggers — is recorded via `rateLimitTracker` when
 * `target.kind === 'provider'`, so a single tracking call site covers both.
 */
export async function attemptStream(
  target: ResolvedTarget,
  wireHistory: ChatMessage[],
  tools: ToolDef[],
  signal: AbortSignal | undefined,
  effort: string | undefined,
  sessionId: string,
  onDelta?: (content: string) => void,
  // Whether a `usage` stream event gets written into `useUsageStore` under
  // `sessionId` — true for every pre-existing caller (the main turn loop,
  // context-trim summarization, the risk judge), so this parameter is
  // additive and none of them had to change. `subagent.ts`'s
  // `runSubagentTask` is the one caller that passes `false`: a child
  // attempt's token usage is real (rateLimitTracker still records it via
  // `recordRequest` below, unconditionally, since it IS a real provider
  // request) but must never clobber the PARENT session's own context-usage
  // ring — see the design doc's "usage clobbering" risk and
  // `subagent.test.ts` for the test pinning this.
  recordUsage: boolean = true
): Promise<AttemptResult> {
  if (target.kind === 'provider') recordRequest(target.providerId);

  let content = '';
  const toolCalls: ToolCall[] = [];
  let streamError: string | null = null;
  let contentStarted = false;

  const events: AsyncGenerator<StreamEvent> =
    target.kind === 'provider'
      ? streamProviderChat(target.providerId, target.model, wireHistory, tools, signal, target.providerId === 'anthropic' ? effort : undefined)
      : streamChat(target.baseUrl, wireHistory, tools, target.kind === 'ollama' ? target.model : undefined, signal);

  try {
    for await (const event of events) {
      if (event.type === 'delta') {
        contentStarted = true;
        content += event.content;
        onDelta?.(content);
      } else if (event.type === 'tool_call') {
        contentStarted = true;
        toolCalls.push(event.toolCall);
      } else if (event.type === 'usage') {
        if (recordUsage) {
          useUsageStore.getState().setUsage(sessionId, {
            promptTokens: event.usage.prompt_tokens,
            completionTokens: event.usage.completion_tokens,
            totalTokens: event.usage.total_tokens,
          });
        }
      }
      // 'done' carries no data; the generator simply returns after it.
    }
  } catch (err) {
    streamError = err instanceof Error ? err.message : String(err);
  }

  return { content, toolCalls, streamError, contentStarted };
}
