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
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { useModelStore } from '../store/modelStore';
import { riskCacheKey, type RiskClassification } from './riskJudge';
import { runSubagentTask } from './subagent';
import { protocolToolCallId } from './durableRun';
import { formatSkillToolResult, type SlashSkill } from './skills';

/** Where a turn's requests should go. Local llama.cpp and Ollama are kept
 * distinct (rather than a single generic "direct fetch" kind) so
 * failover/vision-switch logic can tell exactly which store setter
 * (`useOllamaModel` vs `useProviderModel`) to call when it picks a
 * different target — both still stream via the same `streamChat` transport. */
export type ResolvedTarget =
  | { kind: 'local'; baseUrl: string; modelLabel?: string }
  | { kind: 'ollama'; baseUrl: string; model: string }
  | { kind: 'provider'; providerId: string; model: string };

/** Human-readable label identifying which model a `usage` event belongs to,
 * for the Settings "Usage" tab's per-model breakdown. Local llama.cpp
 * targets carry no model name of their own (see `ResolvedTarget`), so the
 * active model's display name is read from `modelStore` at the moment usage
 * arrives; Ollama/provider targets already carry a model id. */
export function describeUsageTarget(target: ResolvedTarget): string {
  if (target.kind === 'local') return target.modelLabel ?? useModelStore.getState().active?.name ?? 'Local model';
  if (target.kind === 'ollama') return `Ollama · ${target.model}`;
  return `${target.providerId} · ${target.model}`;
}

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
  toolCallId: string,
  mcpRegistry: McpToolRegistry
): Promise<string> {
  const resolved = resolveMcpToolName(mcpRegistry, name);
  if (!resolved) {
    return Promise.resolve(stringifyToolError(new Error(`MCP tool "${name}" was not offered this turn.`)));
  }
  const { turn_id: _turnId, tool_call_id: _toolCallId, ...argumentsForServer } = args;
  return invoke<McpCallToolResult>('mcp_call_tool', {
    server_id: resolved.serverId,
    tool_name: resolved.toolName,
    arguments: argumentsForServer,
    turn_id: turnId,
    tool_call_id: toolCallId,
  }).then(formatMcpCallToolResult, stringifyToolError);
}

/**
 * Whether `toolCall` was actually among the tools offered to the model this
 * turn/run. Lives here (rather than in `agentLoop.ts`, its original home)
 * so `subagent.ts`'s own child tool-calling loop can enforce the exact same
 * gate `agentLoop.ts`'s parent loop does — a real risk with local/quantized
 * models that don't strictly respect the offered tool schema (e.g. an
 * `explore`-profile subagent's model emitting a `write_file` call even
 * though `toolsForProfile('explore')` never offered it). Building the
 * per-turn/per-run tool list only shapes the *schema* sent to the model;
 * this is the enforcement point that makes it an actual authorization
 * boundary rather than a polite suggestion the model can ignore. Re-exported
 * from `agentLoop.ts` for backward compatibility with existing imports/tests.
 */
export function isToolCallAllowed(toolCall: ToolCall, toolsForTurn: ToolDef[]): boolean {
  return toolsForTurn.some((tool) => tool.function.name === toolCall.function.name);
}

/** Tool names eligible for risk classification — see `RiskAnnotationContext`.
 * `run_shell` is included for DISPLAY purposes only (the permission modal can
 * show a badge on a shell prompt too) — see `permissions.rs`'s
 * `tool_run_shell` doc comment for the load-bearing invariant this must never
 * violate: nothing computed here ever feeds into whether `run_shell` (or
 * anything else) gets auto-approved. */
const RISK_ELIGIBLE_TOOLS = new Set(['write_file', 'edit_file', 'run_shell']);

/** Same three tools, under the name that reads right at each of this
 * argument-table's other call sites below (checkpoint_id/agent_label are
 * about "this mutates," not "this gets a risk badge" — they're the same set
 * today, but for a different reason). */
const MUTATING_TOOLS = RISK_ELIGIBLE_TOOLS;

/** Tools whose calls are permission-gated and therefore need `turn_id`, so
 * Rust can scope a permission prompt (and, for run_shell/web_fetch, Stop-button
 * cancellation) to the right in-flight turn. */
const PERMISSION_GATED_TOOLS = new Set([...MUTATING_TOOLS, 'remember', 'web_fetch', 'web_search']);

function isPermissionGatedTool(name: string): boolean {
  return PERMISSION_GATED_TOOLS.has(name) || name.startsWith('mcp__');
}

/** Per-call context `RESERVED_ARGS`' `resolve` functions read from — one
 * object built fresh by `executeToolCall` for each tool call, after risk
 * classification has run (so `riskClassification` reflects this call). */
interface ReservedArgContext {
  name: string;
  checkpointId: string | null;
  turnId: string;
  toolCallId: string;
  agentLabel?: string;
  attachedStackNames?: string[];
  riskClassification: RiskClassification | null;
}

/**
 * The injected-args registry (ROADMAP.md §3 item 3): one table describing
 * every tool-call argument that is frontend-owned — the model must never be
 * able to supply or influence these itself (a model that always claimed
 * "low" risk for its own edits, or forged another subagent's `agent_label`,
 * would defeat the entire point of each being independently controlled).
 * `scrubReservedArgs` deletes every one of these keys unconditionally, for
 * every tool call, before anything (including risk classification) reads
 * `args`; `injectReservedArgs`, called once the risk classification (if any)
 * is known, sets back in only the keys whose `resolve` yields a defined
 * value for this particular call. Replaces what used to be five independent
 * hand-rolled scrub/inject blocks inline in `executeToolCall`.
 */
const RESERVED_ARGS: ReadonlyArray<{ key: string; resolve: (ctx: ReservedArgContext) => unknown }> = [
  // Computed just above the call to `injectReservedArgs` in `executeToolCall`
  // — these two entries just re-attach that result under its two field names.
  { key: 'risk_level', resolve: (ctx) => ctx.riskClassification?.level },
  { key: 'risk_reason', resolve: (ctx) => ctx.riskClassification?.reason },
  // Purely cosmetic subagent attribution (permissions.rs's
  // `PermissionRequestPayload.agent_label`) — never affects which mode
  // auto-approves what (see that field's own doc comment). Only meaningful
  // for the three mutating tools, and only when a caller (`subagent.ts`)
  // actually supplied one — every parent-turn call leaves `agentLabel`
  // `undefined`, so this resolves to `undefined` too and the key is never
  // added at all, not even as an explicit `null`.
  { key: 'agent_label', resolve: (ctx) => (MUTATING_TOOLS.has(ctx.name) ? ctx.agentLabel : undefined) },
  // Pins a pre-mutation backup to the right turn's own checkpoint — the
  // split pane may hold its own concurrent one. `run_shell` doesn't snapshot
  // anything but still gets the id so `record_shell` can flag `shell_ran`.
  {
    key: 'checkpoint_id',
    resolve: (ctx) => (MUTATING_TOOLS.has(ctx.name) && ctx.checkpointId !== null ? ctx.checkpointId : undefined),
  },
  // Scopes permission prompts and shell/fetch cancellation to THIS turn —
  // Stop in one pane must never touch the other pane's command or prompt.
  { key: 'turn_id', resolve: (ctx) => (isPermissionGatedTool(ctx.name) ? ctx.turnId : undefined) },
  // Links Rust-hosted permission decisions to the exact redacted durable
  // ToolProposed event. Provider-supplied ids are normalized before IPC.
  { key: 'tool_call_id', resolve: (ctx) => (isPermissionGatedTool(ctx.name) ? ctx.toolCallId : undefined) },
  // `search_docs` is scoped to THIS session's attached knowledge stacks
  // server-side, never left to the model to declare — always overwritten
  // (even with an empty array), never left merely scrubbed, so a compliant
  // model that omits `stack` still gets the correct default-sweep allow-list
  // (see `stacks.rs`'s `resolve_search_stack_ids` doc comment).
  { key: 'allowed_stack_names', resolve: (ctx) => (ctx.name === 'search_docs' ? ctx.attachedStackNames ?? [] : undefined) },
];

function scrubReservedArgs(args: Record<string, unknown>): void {
  for (const { key } of RESERVED_ARGS) {
    delete args[key];
  }
}

function injectReservedArgs(args: Record<string, unknown>, ctx: ReservedArgContext): void {
  for (const { key, resolve } of RESERVED_ARGS) {
    const value = resolve(ctx);
    if (value !== undefined) {
      args[key] = value;
    }
  }
}

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
  /** Immutable durable parent run id used for permission/cancellation audit. */
  runId?: string;
  /** THIS turn's already-resolved active target (see `ResolvedTarget`) —
   * passed down rather than re-resolved, so a mid-turn manual model switch
   * can never split the parent and child across different targets. */
  target: ResolvedTarget;
  effort?: string;
  /** The parent turn's own risk-annotation context (built once by
   * `agentLoop.ts`'s `runAgentTurnBody`, same object every `executeToolCall`
   * this turn already receives via the `risk` parameter) — threaded through
   * to `runSubagentTask` so a `code`-profile child's write_file/edit_file/
   * run_shell calls get the SAME advisory risk classification (and share the
   * same per-turn cache) the parent's own equivalent calls would, instead of
   * silently skipping classification just because the mutation happened
   * inside a subagent. `undefined` when the caller never built one (risk
   * annotations off, or a future caller that doesn't offer `task` at all). */
  risk?: RiskAnnotationContext;
  /** Called once for every path a `code`-profile child successfully
   * `write_file`/`edit_file`s, so the PARENT turn's own `mutatedFiles`
   * tracking (see `agentLoop.ts`'s `runAgentTurnBody`) sees subagent-driven
   * mutations too — without this, `runVerificationPhase` would never fire
   * for a turn where every mutation happened inside a delegated `task` call,
   * since the parent round's own `toolCalls` array only ever contains the
   * single `task` entry, never the child's nested write_file/edit_file
   * calls. `undefined` for a caller that doesn't track mutated files at all. */
  onMutatedPath?: (path: string) => void;
}

/**
 * Everything `executeToolCall` needs to resolve a model-requested `skill`
 * tool call (see `tools.ts`'s `SKILL_INVOKE_TOOL`) against the turn's own
 * available skills — built once per turn by `agentLoop.ts`'s
 * `runAgentTurnBody`, same "built once, passed down unchanged" shape as
 * `RiskAnnotationContext`/`SubagentContext`, and passed through every
 * `executeToolCall` call this turn.
 */
export interface SkillToolContext {
  /** Every skill installed/enabled this turn — the same list
   * `composeSkillCatalog` draws its "## Available skills" listing from. */
  availableSkills: SlashSkill[];
  /** Commands already invoked this turn — explicit `/command` invocations
   * (present before the loop even starts) plus every command a previous
   * `skill` tool call in THIS turn already resolved. Mutated in place (not
   * replaced) so later iterations of the same turn see earlier model-invoked
   * skills too, exactly like `mutatedFiles`/`onMutatedPath` in
   * `agentLoop.ts`. A duplicate invocation of an already-present command is
   * rejected with a tool error rather than silently re-returning the same
   * instructions again. */
  invokedCommands: Set<string>;
  /** Hard cap on total skills (explicit + model-invoked) per turn — mirrors
   * `skills.ts`'s `MAX_SKILLS_PER_TURN`, the same bound `parseSkillTurn`
   * already enforces for stacked explicit invocations. */
  maxSkillsPerTurn: number;
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
  subagent?: SubagentContext,
  // Subagent-attribution label (slice 3) — see the injection site below.
  // Threaded through to the Rust command as its own `agent_label` field,
  // which `permissions::request_permission` forwards as its own
  // `PermissionRequestPayload.agent_label` field (NOT folded into `detail`
  // as a parsed-out-by-regex prefix — see that field's doc comment for why
  // that earlier design was a spoofing/corruption bug). Undefined for every
  // parent-turn call (`agentLoop.ts` never passes it); `subagent.ts`'s
  // `runSubagentTask` passes its own `description` here for each of ITS
  // child's tool calls.
  agentLabel?: string,
  // Context for resolving a model-requested `skill` tool call — see
  // `SkillToolContext`'s doc comment. `undefined` for any caller that never
  // offers the `skill` tool (e.g. `crewRunner.ts`/`subagent.ts`, which don't
  // thread skill auto-invocation through at all — see agentLoop.ts's plan
  // doc for why that's out of scope for now); a `skill` call reaching
  // `executeToolCall` with no context configured is defensively reported as
  // a tool error rather than throwing, same posture as an unconfigured
  // `subagent` reaching the `task` branch below.
  skill?: SkillToolContext
): Promise<string> {
  useUsageHistoryStore.getState().recordToolCall();
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

  // See `RESERVED_ARGS`'s doc comment: every frontend-owned key is scrubbed
  // unconditionally, for every tool, before anything else (including risk
  // classification below) ever reads `args` — so there is no code path where
  // a model-supplied value for any of them survives.
  scrubReservedArgs(args);

  // Classify (cached per turn) on the now-scrubbed args, BEFORE
  // `injectReservedArgs` below, so both the cache key and the judge prompt
  // reflect only the model's actual call — not internal bookkeeping fields
  // it never provided.
  let riskClassification: RiskClassification | null = null;
  if (RISK_ELIGIBLE_TOOLS.has(name) && risk?.enabled) {
    const key = riskCacheKey(name, args);
    if (risk.cache.has(key)) {
      riskClassification = risk.cache.get(key) ?? null;
    } else {
      riskClassification = await risk.classify(name, args);
      risk.cache.set(key, riskClassification);
    }
  }

  // Sets back in only the keys that apply to this tool call, from the
  // frontend's own sources of truth — see `RESERVED_ARGS`.
  injectReservedArgs(args, {
    name,
    checkpointId,
    turnId,
    toolCallId: protocolToolCallId(toolCall.id),
    agentLabel,
    attachedStackNames,
    riskClassification,
  });

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
        runId: subagent.runId,
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
        risk: subagent.risk,
        onMutatedPath: subagent.onMutatedPath,
      });
    } catch (err) {
      return stringifyToolError(err);
    }
  }

  // `skill` is a third frontend-only tool, same treatment as `present_plan`/
  // `task` above: no `tool_skill` Rust command exists, so it's intercepted
  // here rather than falling through to the `invoke`/`mcp__` dispatch below.
  // Wrapped in try/catch for the same reason as the `task` branch — any
  // exception here must still produce a matching `tool` result rather than
  // leaving this call's `tool_calls` entry without one.
  if (name === 'skill') {
    try {
      if (!skill) {
        return stringifyToolError(new Error('The skill tool has no context configured for this turn.'));
      }
      const command = typeof args.command === 'string' ? args.command.trim().replace(/^\//, '').toLowerCase() : '';
      if (!command) {
        return stringifyToolError(new Error('The skill tool requires a "command" argument.'));
      }
      if (skill.invokedCommands.has(command)) {
        return stringifyToolError(new Error(`/${command} was already invoked this turn.`));
      }
      if (skill.invokedCommands.size >= skill.maxSkillsPerTurn) {
        return stringifyToolError(new Error(`A turn can invoke at most ${skill.maxSkillsPerTurn} skills.`));
      }
      const matched = skill.availableSkills.find((candidate) => candidate.command.toLowerCase() === command);
      if (!matched) {
        return stringifyToolError(new Error(`No enabled skill named "/${command}".`));
      }
      // Recorded BEFORE returning the result (not after) so a second call
      // for the same command later in this same batch of parallel tool
      // calls is still caught by the duplicate check above — `Promise.all`
      // in `agentLoop.ts` runs these concurrently, but each `skill` branch
      // still executes its own body serially with respect to this
      // synchronous bookkeeping (no `await` happens between the checks above
      // and this line).
      skill.invokedCommands.add(command);
      const argumentsText = typeof args.arguments === 'string' ? args.arguments : '';
      return formatSkillToolResult(matched, argumentsText);
    } catch (err) {
      return stringifyToolError(err);
    }
  }

  // `read_skill_resource` has a real `tool_read_skill_resource` Rust command
  // (see `tools.ts`'s `READ_SKILL_RESOURCE_TOOL` doc comment), so unlike
  // `skill` above it isn't intercepted — but its own schema promises it
  // "only works for a skill that has already been invoked this turn", so
  // that must be checked here before falling through to the generic
  // `invoke` dispatch below, rather than trusting the model to only ever
  // call it post-invocation.
  if (name === 'read_skill_resource') {
    if (!skill) {
      return stringifyToolError(new Error('The read_skill_resource tool has no context configured for this turn.'));
    }
    const command = typeof args.command === 'string' ? args.command.trim().replace(/^\//, '').toLowerCase() : '';
    if (!command || !skill.invokedCommands.has(command)) {
      return stringifyToolError(
        new Error(`/${command || '(missing)'} has not been invoked this turn — invoke it via the skill tool or /command first.`)
      );
    }
  }

  const invocation = name.startsWith('mcp__')
    ? invokeMcpTool(name, args, turnId, protocolToolCallId(toolCall.id), mcpRegistry)
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
export interface AttemptResult {
  content: string;
  toolCalls: ToolCall[];
  streamError: string | null;
  /** Whether any content/tool-call fragment arrived before `streamError` (if any) — the failover safety rule below only ever retries a *different* target when this is `false`, since a mid-stream error has already shown the user partial output that a retry could duplicate or contradict. */
  contentStarted: boolean;
  /** The raw token counts from this attempt's own `usage` stream event, if
   * one arrived — populated regardless of `recordUsage` (see that param's
   * doc comment): `recordUsage` only gates whether `useUsageStore` gets
   * written, it must never gate whether the CALLER can see its own attempt's
   * usage. `subagent.ts`'s `runSubagentTask` is the one caller that reads
   * this (slice 4, per-subagent token usage surfaced in `SubagentRow`) —
   * every pre-existing caller already gets the same numbers via
   * `useUsageStore` and simply ignores this field. `undefined` when no
   * `usage` event arrived at all (e.g. a provider that doesn't report it, or
   * a stream that errored before one showed up). */
  usage?: { promptTokens: number; completionTokens: number; totalTokens: number };
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
  recordUsage: boolean = true,
  /** Optional hard completion ceiling for compatible local/Ollama routes.
   * Provider calls are still bounded by the caller's AbortController and
   * aggregate ledger because their Rust proxy contract does not yet expose
   * this optional field. */
  maxTokens?: number,
  /** Durable run whose host-canonicalized provider endpoint/model must match
   * this request. Ignored by unauthenticated local runtimes. */
  runId?: string,
): Promise<AttemptResult> {
  if (target.kind === 'provider') recordRequest(target.providerId);

  let content = '';
  const toolCalls: ToolCall[] = [];
  let streamError: string | null = null;
  let contentStarted = false;
  let usage: AttemptResult['usage'];

  const events: AsyncGenerator<StreamEvent> =
    target.kind === 'provider'
      ? streamProviderChat(
          target.providerId,
          target.model,
          wireHistory,
          tools,
          signal,
          // Forwarded for every provider target — the Rust proxy owns the
          // per-provider wire mapping/omission (verbatim for Anthropic,
          // clamped reasoning_effort for OpenAI/Gemini/OpenRouter, nothing
          // for custom endpoints; see providers.rs::build_chat_request).
          effort,
          runId,
        )
      : streamChat(
          target.baseUrl,
          wireHistory,
          tools,
          target.kind === 'ollama' ? target.model : undefined,
          signal,
          maxTokens,
        );

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
        usage = {
          promptTokens: event.usage.prompt_tokens,
          completionTokens: event.usage.completion_tokens,
          totalTokens: event.usage.total_tokens,
        };
        if (recordUsage) {
          useUsageStore.getState().setUsage(sessionId, usage);
          useUsageHistoryStore.getState().recordUsage(describeUsageTarget(target), usage);
        }
      }
      // 'done' carries no data; the generator simply returns after it.
    }
  } catch (err) {
    streamError = err instanceof Error ? err.message : String(err);
  }

  return { content, toolCalls, streamError, contentStarted, usage };
}
