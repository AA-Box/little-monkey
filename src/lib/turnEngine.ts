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

/** Resolves when `signal` aborts (never resolves for an undefined signal). */
function abortedPromise(signal: AbortSignal): Promise<void> {
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
  signal?: AbortSignal
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
  onDelta?: (content: string) => void
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
        useUsageStore.getState().setUsage(sessionId, {
          promptTokens: event.usage.prompt_tokens,
          completionTokens: event.usage.completion_tokens,
          totalTokens: event.usage.total_tokens,
        });
      }
      // 'done' carries no data; the generator simply returns after it.
    }
  } catch (err) {
    streamError = err instanceof Error ? err.message : String(err);
  }

  return { content, toolCalls, streamError, contentStarted };
}
