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
import type { RoutingDecision } from './modelRouting';
import type { ChatMessage, StreamEvent, ToolCall, ToolDef } from './llamaClient';
import { streamProviderChat } from './providerClient';
import { formatMcpCallToolResult, resolveMcpToolName, type McpCallToolResult, type McpToolRegistry } from './mcpTools';
import {
  invokeExecutableExtensionTool,
  type ExtensionToolRegistry,
} from './executableExtensionTools';
import { classifyExternalTool } from './checkpointReconciliation';
import { recordRequest } from './rateLimitTracker';
import { useUsageStore } from '../store/usageStore';
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { useTurnStatusStore } from '../store/turnStatusStore';
import { useTaskSuggestionStore } from '../store/taskSuggestionStore';
import { useModelStore } from '../store/modelStore';
import {
  assertCostBudgetAllowsRequest,
  calculateUsageCostUsd,
  useCostControlStore,
} from '../store/costControlStore';
import {
  localModelTargetKey,
  ollamaModelTargetKey,
  providerModelTargetKey,
} from './modelTargets';
import { riskCacheKey, type RiskClassification } from './riskJudge';
import { evaluatePreToolUseHooks, fireObservedHooks, hooksForEvent } from './userHooks';
import { gatePrivacyWireMessages, type PrivacyWireCache } from './privacyWire';
import { usePrivacyFirewallStore } from '../store/privacyFirewallStore';
import { primaryRoot, useWorkspaceStore } from '../store/workspaceStore';
import { useSessionStore } from '../store/sessionStore';
import { usePermissionStore } from '../store/permissionStore';
import { runSubagentTask } from './subagent';
import { resolveWorkflowSpec, runWorkflow } from './workflow';
import { protocolToolCallId } from './durableRun';
import { formatSkillToolResult, type SlashSkill } from './skills';
import { rasterizeSvgToPng, type RasterizedPng } from './imageGeneration';
import { errorMessage } from "./errors";

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

function costTargetKey(target: ResolvedTarget): string {
  if (target.kind === 'provider') {
    return providerModelTargetKey(target.providerId, target.model);
  }
  if (target.kind === 'ollama') return ollamaModelTargetKey(target.model);
  return localModelTargetKey(
    useModelStore.getState().active?.id ?? target.modelLabel ?? 'local',
  );
}

/**
 * Where to charge this request (K25).
 *
 * `workspacePath` is the folder open right now — the same identity the K6
 * process ledger stamps on the processes this turn will spawn, which is what
 * lets a workspace's token bill and its device time be added up together.
 * `projectPath` is the folder the *conversation* belongs to, snapshotted when
 * the session was created: a chat resumed after the user switched folders is
 * still that project's cost, and charging it to today's folder would be wrong.
 */
function attributionOf(sessionId: string): {
  workspacePath: string | null;
  projectPath: string | null;
} {
  return {
    workspacePath: primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null,
    projectPath:
      useSessionStore.getState().sessions.find((session) => session.id === sessionId)
        ?.workspacePath ?? null,
  };
}

function isMeteredTarget(target: ResolvedTarget): boolean {
  if (target.kind === 'provider') return true;
  if (target.kind !== 'ollama') return false;
  return (
    useModelStore
      .getState()
      .ollamaModels
      .find((model) => model.name === target.model)
      ?.is_cloud === true
  );
}

/** Stringifies a tool invocation's result (or error) for use as tool-message content. */
export function stringifyToolResult(result: unknown): string {
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
/** How many streamed characters accumulate before the status line's live
 * token estimate is updated — roughly 50 tokens, which is under one tick of
 * the 0.1k the label rounds to, so batching costs nothing visible. */
const STATUS_CHAR_FLUSH = 200;

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

/**
 * Whether Plan Mode refuses `name` outright: every permission-gated tool
 * (the exact set Rust's `mode_short_circuit` would refuse anyway — this
 * predicate is the fail-closed frontend mirror, used both to EXCLUDE these
 * names from the offered list (`toolsForMode`, agentLoop.ts) and as
 * `executeToolCall`'s dispatch backstop), plus:
 * - `shell_kill`: not permission-gated in Rust (a prompt per kill would make
 *   the Stop affordance useless), so the frontend layers here are its only
 *   Plan-Mode guard — it mutates process state, which planning never needs.
 * - every `mcp__` name: MCP tools carry no reliable read-only marking, so
 *   Plan Mode excludes them wholesale rather than guessing which ones only
 *   read (they are all permission-gated in Rust too, so this tightens the
 *   offer to match what dispatch would do anyway).
 * Exported for `toolsForMode` and the logic tests.
 */
export function isBlockedInPlanMode(name: string): boolean {
  return PERMISSION_GATED_TOOLS.has(name) || name === 'shell_kill' || name.startsWith('mcp__') || name.startsWith('ext__');
}

function isPermissionGatedTool(name: string): boolean {
  return PERMISSION_GATED_TOOLS.has(name) || name.startsWith('mcp__') || name.startsWith('ext__');
}

async function extensionInvocationId(
  turnOrRunId: string,
  providerToolCallId: string,
  binding: { extensionId: string; capabilityId: string; version: string },
): Promise<string> {
  const identity = JSON.stringify([
    turnOrRunId,
    providerToolCallId,
    binding.extensionId,
    binding.capabilityId,
    binding.version,
  ]);
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(identity));
  const hex = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `ext-inv-${hex}`;
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
  /** Managed agent-worktree path a worktree-isolated subagent's fs/shell
   * calls resolve against — see `WORKTREE_OVERRIDE_TOOLS`. */
  workspaceRootOverride?: string;
}

/** The tools whose path/cwd resolution honours a worktree override — the
 * child profiles' fs tools plus run_shell. Everything else (web, memory,
 * MCP) has no workspace path to redirect. */
const WORKTREE_OVERRIDE_TOOLS: ReadonlySet<string> = new Set([
  'read_file',
  'list_dir',
  'glob',
  'grep',
  'write_file',
  'edit_file',
  'run_shell',
]);

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
  // split pane may hold its own concurrent one. The mutating tools need it to
  // snapshot; the external-effect tools need it for a different reason and get
  // it too, since `checkpoints.rs`'s `record_external_effect` is what makes a
  // network/MCP/memory effect survive context compaction. Without the id those
  // effects exist only in the transcript, and `contextTrimmer.ts` can drop that
  // — after which a rollback reported nothing to reconcile.
  {
    key: 'checkpoint_id',
    resolve: (ctx) =>
      (MUTATING_TOOLS.has(ctx.name) || classifyExternalTool(ctx.name) !== null) && ctx.checkpointId !== null
        ? ctx.checkpointId
        : undefined,
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
  // Points a worktree-isolated subagent's fs/shell calls at ITS worktree —
  // frontend-owned like everything here (a model-supplied value is scrubbed),
  // and Rust additionally refuses any value that isn't a registered agent
  // worktree (`agent_worktrees::resolve_with_override`), so even a forged
  // value could only ever name a directory this app created for this purpose.
  { key: 'workspace_root_override', resolve: (ctx) => (WORKTREE_OVERRIDE_TOOLS.has(ctx.name) ? ctx.workspaceRootOverride : undefined) },
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
  /** Shared id for THIS round's parallel `task` calls — set by
   * `agentLoop.ts` only when the round carries two or more of them (a fresh
   * UUID per round, since provider-fallback tool-call ids repeat), and
   * threaded through to `runSubagentTask` as its `groupId` so the
   * Background-tasks drawer can render the round as one grouped card.
   * `undefined` for a lone `task` call. */
  taskGroupId?: string;
  /** THIS turn's already-resolved active target (see `ResolvedTarget`) —
   * passed down rather than re-resolved, so a mid-turn manual model switch
   * can never split the parent and child across different targets. */
  target: ResolvedTarget;
  effort?: string;
  /** Called once with the child's K9 dispatch decision, so it reaches the
   * ledger on the parent's run.
   *
   * A callback for the reason `onMutatedPath` is one: `subagent.ts` cannot
   * import `agentLoop.ts` (this module imports `subagent.ts`, so the edge
   * closes a cycle), and the recorder lives there. A subagent has no durable
   * run of its own — it borrows the parent's `runId` for permission and
   * cancellation audit already — so the parent's run is where its decision
   * belongs. `undefined` when the caller has no recorder, which is the same
   * shape every other optional hook here takes. */
  onRoutingDecision?: (decision: RoutingDecision) => void;
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
  /** Called for every failed child `write_file`/`edit_file` so the parent
   * mutation contract cannot mistake one successful child edit for complete
   * success when another requested edit was denied or failed. */
  onMutationFailure?: (
    path: string | null,
    reason: string,
    toolCallId: string,
  ) => void;
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
 *
 * User hooks wrap the whole dispatch (see `userHooks.ts`): a PreToolUse
 * hook that explicitly denies blocks the call BEFORE any dispatch and its
 * reason becomes the tool error; PostToolUse hooks observe the result and
 * can change nothing. A hook that crashes or times out is a console WARN
 * and the call proceeds — the deny path fails closed only on an explicit
 * deny, never on hook infrastructure failure. Every caller (parent turns,
 * subagents, workflow agents, crew) passes through here, so the hook
 * boundary is a single place by construction.
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
  agentLabel?: string,
  skill?: SkillToolContext,
  chatSessionId?: string,
  // Managed agent-worktree root for this call's fs/shell resolution — see
  // `executeToolCallInner`'s param of the same name.
  workspaceRootOverride?: string,
  extensionRegistry?: ExtensionToolRegistry,
): Promise<string> {
  const name = toolCall.function.name;
  const sessionId = chatSessionId ?? subagent?.sessionId;
  // Hook payloads carry the model's own args — parsed leniently here (the
  // strict parse with its error result stays in the inner dispatch).
  const argsForHooks = (): Record<string, unknown> => {
    try {
      const parsed: unknown = JSON.parse(toolCall.function.arguments || '{}');
      return parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {};
    } catch {
      return {};
    }
  };

  if (hooksForEvent('PreToolUse', name).length > 0) {
    try {
      const denial = await evaluatePreToolUseHooks(name, argsForHooks(), sessionId);
      if (denial !== null) {
        return stringifyToolError(new Error(`Blocked by a PreToolUse hook: ${denial.reason}`));
      }
    } catch (err) {
      // The evaluator itself already proceeds past individual hook failures;
      // this catch is defense in depth so a hook-layer bug can never leave a
      // tool_calls entry without a result.
      console.warn('PreToolUse hook evaluation failed — proceeding:', err);
    }
  }

  const result = await executeToolCallInner(
    toolCall,
    checkpointId,
    turnId,
    mcpRegistry,
    signal,
    risk,
    attachedStackNames,
    subagent,
    agentLabel,
    skill,
    chatSessionId,
    workspaceRootOverride,
    extensionRegistry,
  );

  if (hooksForEvent('PostToolUse', name).length > 0) {
    fireObservedHooks('PostToolUse', { tool_name: name, args: argsForHooks(), session_id: sessionId, result });
  }
  return result;
}

async function executeToolCallInner(
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
  skill?: SkillToolContext,
  // The chat session this turn belongs to — needed only by the frontend-only
  // `spawn_task` tool, whose chips render under one specific transcript.
  // Omitted by every runner that doesn't offer `spawn_task` (subagents, side
  // tasks, crew); a `spawn_task` call arriving without it is reported as a
  // tool error rather than guessing which conversation to attach a chip to.
  chatSessionId?: string,
  // Managed agent-worktree path this call's fs/shell tools resolve against —
  // supplied ONLY by `runSubagentTask` for a worktree-isolated child, per
  // call rather than via any global state, so concurrent isolated agents can
  // never race each other's roots. See the `workspace_root_override`
  // RESERVED_ARGS entry for the trust story.
  workspaceRootOverride?: string,
  extensionRegistry?: ExtensionToolRegistry,
): Promise<string> {
  useUsageHistoryStore.getState().recordToolCall();
  const { name, arguments: rawArguments } = toolCall.function;

  // Plan Mode backstop — the frontend half of the double layer whose other
  // half is Rust's `mode_short_circuit` (permissions.rs): the offered tool
  // list already excludes these names in Plan Mode (`toolsForMode`,
  // agentLoop.ts), but a model that emits one anyway (or a child loop whose
  // caller composed its own list) must still be refused HERE, before any
  // dispatch. This is also the ONLY enforcement point for the names Rust
  // doesn't permission-gate (`shell_kill`), and it covers every subagent's
  // calls too, since `runSubagentTask` routes through this function.
  // Checked before the frontend-only branches below purely because none of
  // those names are blocked — the message mirrors Rust's own wording.
  if (isBlockedInPlanMode(name) && usePermissionStore.getState().mode === 'plan') {
    return stringifyToolError(
      new Error(
        `Blocked: Little Monkey is in Plan Mode. Describe your plan instead of using ${name} - call the present_plan tool with your proposed plan, then ask the user to approve it and switch out of Plan Mode before making changes.`
      )
    );
  }

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
    workspaceRootOverride,
  });

  // `present_plan` is a frontend-only tool (see `tools.ts`'s `PRESENT_PLAN_TOOL`
  // doc comment): it never reaches Rust at all, checked BEFORE the
  // mcp__/tool_<name> dispatch below so this holds regardless of what future
  // caller invokes `executeToolCall` for it, not just today's single call
  // site in `agentLoop.ts`.
  if (name === 'present_plan') {
    return PRESENT_PLAN_RESULT;
  }

  // `spawn_task` is frontend-only too, and deliberately inert: it stages a
  // SUGGESTION chip under this session's transcript and returns. No model
  // call, no session, no side effect happens until the user clicks the chip
  // (see `taskSuggestionStore.ts`), which is why it needs no permission
  // prompt — the model is proposing follow-up work, not starting it.
  if (name === 'spawn_task') {
    if (!chatSessionId) {
      return stringifyToolError(new Error('The spawn_task tool is not available in this context.'));
    }
    const title = typeof args.title === 'string' ? args.title : '';
    const prompt = typeof args.prompt === 'string' ? args.prompt : '';
    if (!title.trim() || !prompt.trim()) {
      return stringifyToolError(new Error('spawn_task requires non-empty "title" and "prompt" arguments.'));
    }
    const suggestion = useTaskSuggestionStore.getState().create({
      sessionId: chatSessionId,
      title,
      tldr: typeof args.tldr === 'string' ? args.tldr : '',
      prompt,
    });
    // Recorded *after* the store assigned the id, for `tool_remember`'s reason:
    // an id guessed beforehand could name a chip that was never created. One
    // call rather than two, so the enumerated effect and the id it needs to
    // withdraw can never be recorded by halves.
    if (checkpointId) {
      await invoke('checkpoint_record_task_suggestion', {
        id: checkpointId,
        suggestionId: suggestion.id,
      }).catch(() => undefined);
    }
    return stringifyToolResult({
      task_id: suggestion.id,
      status: 'suggested',
      note: 'A chip was shown to the user. Nothing runs unless they click it — keep working on the current task.',
    });
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
      // Passed through as a string: besides the built-in 'explore'/'code',
      // a loaded custom agent's name is valid too — `runSubagentTask`'s
      // `resolveSubagentProfile` is the single validation point, and an
      // unknown name comes back as a tool error naming the known profiles
      // (never a silent fallback that would run a mutating task under the
      // wrong tool set).
      const profile = typeof args.profile === 'string' && args.profile.trim().length > 0 ? args.profile.trim() : 'explore';
      // Validated properly inside `runSubagentTask` (code-class profiles
      // only) — anything but the literal 'worktree' is simply absent.
      const isolation: 'worktree' | undefined = args.isolation === 'worktree' ? 'worktree' : undefined;
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
        groupId: subagent.taskGroupId,
        description,
        prompt: taskPrompt,
        profile,
        isolation,
        target: subagent.target,
        effort: subagent.effort,
        onRoutingDecision: subagent.onRoutingDecision,
        risk: subagent.risk,
        onMutatedPath: subagent.onMutatedPath,
        onMutationFailure: subagent.onMutationFailure,
      });
    } catch (err) {
      return stringifyToolError(err);
    }
  }

  // `workflow` is the named, phased counterpart of `task` — same frontend-only
  // interception, same SubagentContext requirement (which is also what keeps a
  // child loop from ever running one: `runSubagentTask`'s own dispatch never
  // configures the context, and `toolsForProfile` never offers the name).
  // Same whole-branch try/catch as `task`, for the same transcript-validity
  // invariant.
  if (name === 'workflow') {
    try {
      if (!subagent) {
        return stringifyToolError(new Error('The workflow tool has no subagent execution context configured for this turn.'));
      }
      const spec = resolveWorkflowSpec(args);
      return await runWorkflow({
        sessionId: subagent.sessionId,
        runId: subagent.runId,
        parentCheckpointId: checkpointId,
        parentSignal: signal,
        toolCallId: toolCall.id,
        spec,
        resume: typeof args.resume === 'string' && args.resume.trim().length > 0 ? args.resume.trim() : undefined,
        target: subagent.target,
        effort: subagent.effort,
        risk: subagent.risk,
        onRoutingDecision: subagent.onRoutingDecision,
        onMutatedPath: subagent.onMutatedPath,
        onMutationFailure: subagent.onMutationFailure,
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

  // `generate_image` dispatches to a real `tool_generate_image` Rust command,
  // but can't take the generic pass-through below: the model supplies SVG
  // markup, and only the webview can rasterize it into the PNG bytes the
  // Rust command actually persists (see `GENERATE_IMAGE_TOOL`'s doc comment
  // in tools.ts). Arguments are re-shaped here — `svg` is consumed by the
  // rasterizer and replaced with frontend-computed `content_base64`/`width`/
  // `height`. The Rust command writes only to app-owned artifact storage, so
  // no workspace path, permission context, or checkpoint id is injected.
  if (name === 'generate_image') {
    const filename = typeof args.filename === 'string'
      ? args.filename.trim()
      : typeof args.path === 'string'
        ? args.path.trim()
        : '';
    const svg = typeof args.svg === 'string' ? args.svg.trim() : '';
    if (!filename || !svg) {
      return stringifyToolError(new Error('generate_image requires both "filename" and "svg" string arguments.'));
    }
    if (!filename.toLowerCase().endsWith('.png')) {
      return stringifyToolError(new Error(`"${filename}" must end in .png — generate_image always produces a PNG file.`));
    }

    let raster: RasterizedPng;
    try {
      raster = await rasterizeSvgToPng(svg);
    } catch (err) {
      return stringifyToolError(err);
    }
    // Rasterization isn't Rust-cancellable, so Stop during it is honored
    // here, before the durable artifact write begins.
    if (signal?.aborted) return CANCELLED_TOOL_RESULT;

    const passthrough = { ...args };
    delete passthrough.svg;
    delete passthrough.filename;
    delete passthrough.path;
    const invocation = invoke('tool_generate_image', {
      ...passthrough,
      filename,
      content_base64: raster.contentBase64,
      width: raster.width,
      height: raster.height,
    }).then(stringifyToolResult, stringifyToolError);
    return raceInvocationWithStop(invocation, turnId, signal);
  }

  // Background shell commands: one tool name (`run_shell`), two Rust
  // commands. `run_in_background` selects the second — a process owned by
  // `background_shell.rs` that outlives this turn — instead of the
  // timeout-capped, killed-on-drop foreground child. The permission gate,
  // and every frontend-injected arg above (checkpoint/turn/tool-call ids,
  // the display-only risk annotation), is identical on both sides, so the
  // flag changes the process's lifetime and nothing about its authority.
  if (name === 'run_shell') {
    const background = args.run_in_background === true;
    const passthrough = { ...args };
    delete passthrough.run_in_background;
    const invocation = invoke(background ? 'tool_run_shell_background' : 'tool_run_shell', passthrough).then(
      stringifyToolResult,
      stringifyToolError,
    );
    return raceInvocationWithStop(invocation, turnId, signal);
  }

  // The two background-task companions to `run_shell`. Both read or stop a
  // process the user can already see in the Background Tasks panel, so
  // neither is permission-gated (nothing new is granted by them) and neither
  // needs the Stop race: they return immediately. Their Rust commands aren't
  // `tool_`-prefixed, so they can't fall through to the generic dispatch.
  if (name === 'shell_output' || name === 'shell_kill') {
    const id = typeof args.id === 'string' ? args.id.trim() : '';
    if (!id) return stringifyToolError(new Error(`${name} requires an "id" argument (the background task id from run_shell).`));
    return name === 'shell_output'
      ? invoke('background_shell_output', { id, drain: args.drain === false ? false : true }).then(
          stringifyToolResult,
          stringifyToolError,
        )
      : invoke('background_shell_kill', { id }).then(stringifyToolResult, stringifyToolError);
  }

  const extensionBinding = extensionRegistry?.get(name);
  const durableExtensionInvocationId = extensionBinding
    ? await extensionInvocationId(turnId, toolCall.id, extensionBinding)
    : undefined;
  const invocation = name.startsWith('mcp__')
    ? invokeMcpTool(name, args, turnId, protocolToolCallId(toolCall.id), mcpRegistry)
    : name.startsWith('ext__')
      ? invokeExecutableExtensionTool(
          name,
          args,
          durableExtensionInvocationId ?? protocolToolCallId(`${turnId}:${toolCall.id}:${name}`),
          extensionRegistry ?? new Map(),
        )
          .then((result) => result.tool_result ?? result.output_json, stringifyToolError)
      : invoke(`tool_${name}`, args).then(stringifyToolResult, stringifyToolError);
  return raceInvocationWithStop(
    invocation,
    turnId,
    signal,
    name.startsWith('ext__') ? durableExtensionInvocationId : undefined,
  );
}

/** Races an in-flight tool `invoke` against the Stop button: on abort, the
 * Rust side is told to cancel everything cancellable (`tools_cancel_running`
 * kills any running shell child and denies any pending permission prompt)
 * and a cancelled result is returned immediately rather than waiting the
 * command out. The original invocation promise already has handlers attached
 * (never an unhandled rejection) and its eventual result is simply
 * discarded. Extracted verbatim from `executeToolCall`'s tail so the
 * `generate_image` interception branch shares the exact same race/cancel
 * shape instead of hand-rolling its own. */
async function raceInvocationWithStop(
  invocation: Promise<string>,
  turnId: string,
  signal?: AbortSignal,
  extensionInvocationId?: string,
): Promise<string> {
  if (!signal) return invocation;

  const raced = await Promise.race([invocation, abortedPromise(signal).then(() => null)]);
  if (raced !== null) return raced;

  void invoke('tools_cancel_running', { turnId }).catch(() => {});
  if (extensionInvocationId) {
    void invoke('extensions_cancel', { invocationId: extensionInvocationId }).catch(() => {});
  }
  return CANCELLED_TOOL_RESULT;
}

/**
 * How `attemptStream` applies the Privacy Firewall to an imminent
 * provider-bound request. The firewall check lives INSIDE `attemptStream` —
 * the one choke point every cloud request in the app flows through — so a
 * surface that forgets to think about privacy (Compare, Crew, side tasks,
 * subagents, translation, the eval judge, and every one-shot workbench
 * flow) is gated by default rather than silently exempt.
 */
export interface AttemptPrivacyOptions {
  /**
   * The caller already passed this exact wire payload through
   * `gatePrivacyWireMessages` (agentLoop's main turn/failover/compaction/
   * judge paths do, because they own richer outcomes like switching the
   * whole turn to a local model). Skips the redundant second scan.
   */
  preGated?: boolean;
  /**
   * Redaction cache shared across related attempts (e.g. one Compare run's
   * four targets), so identical history is scanned/approved once. Scoped to
   * a run on purpose — a longer-lived cache would let a stale "send"
   * decision outlive a policy change.
   */
  cache?: PrivacyWireCache;
}

/**
 * Applies the Privacy Firewall to a provider-bound wire copy. Returns the
 * (possibly redacted) messages to send, or an `AttemptResult`-shaped refusal
 * when the outcome forbids sending. A preview/gate failure fails CLOSED:
 * nothing is sent on an error, because "the scanner broke" must never mean
 * "the protected content left the machine anyway".
 */
async function gateProviderWire(
  messages: ChatMessage[],
  cache: PrivacyWireCache,
): Promise<{ ok: true; messages: ChatMessage[] } | { ok: false; refusal: AttemptResult }> {
  const workspaceId = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? 'global';
  const refusal = (streamError: string): { ok: false; refusal: AttemptResult } => ({
    ok: false,
    refusal: { content: '', toolCalls: [], streamError, contentStarted: false },
  });
  let outcome: Awaited<ReturnType<typeof gatePrivacyWireMessages>>;
  try {
    outcome = await gatePrivacyWireMessages(
      messages,
      (content) => usePrivacyFirewallStore.getState().gateOutbound(content, 'cloud_model', workspaceId),
      cache,
    );
  } catch (error) {
    return refusal(
      `Privacy Firewall could not inspect this request, so nothing was sent: ${errorMessage(error)}`,
    );
  }
  if (outcome.action === 'cancelled') {
    return refusal('Privacy Firewall blocked protected content from leaving the machine; the request was cancelled before anything was sent.');
  }
  if (outcome.action === 'switch_local') {
    return refusal('Privacy Firewall requested a local model for this content. This surface cannot switch targets automatically — nothing was sent; pick a local target and retry.');
  }
  return { ok: true, messages: outcome.messages };
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
  /** Whether this attempt's usage also counts toward the chat's live "✳ …
   * N tokens" status line (`turnStatusStore`). True for everything that IS
   * the turn's own work (main-loop attempts, failovers, context-trim
   * summarization); the risk judge passes `false` — its side-channel
   * classification calls run mid-turn under the same `sessionId` with
   * `recordUsage: true`, and would otherwise silently inflate the label. */
  recordTurnStatusTokens: boolean = true,
  /** See {@link AttemptPrivacyOptions}. Omitted → gate here with a fresh
   * per-attempt cache, which is the safe default for every one-shot caller. */
  privacy?: AttemptPrivacyOptions,
): Promise<AttemptResult> {
  if (target.kind === 'provider') {
    try {
      assertCostBudgetAllowsRequest(useCostControlStore.getState());
    } catch (error) {
      return {
        content: '',
        toolCalls: [],
        streamError: errorMessage(error),
        contentStarted: false,
      };
    }
    if (!privacy?.preGated) {
      const gated = await gateProviderWire(wireHistory, privacy?.cache ?? new Map());
      if (!gated.ok) return gated.refusal;
      wireHistory = gated.messages;
    }
    recordRequest(target.providerId);
  }

  let content = '';
  const toolCalls: ToolCall[] = [];
  let streamError: string | null = null;
  let contentStarted = false;
  let usage: AttemptResult['usage'];
  // Time-to-first-token for K9's latency criterion (`modelRouting.ts`), the
  // only latency signal this app will route on. Started immediately before
  // the loop rather than at function entry: both stream generators are lazy,
  // so nothing is sent until the first `next()` below, and starting the clock
  // earlier would charge this target for the privacy gate and budget check.
  let firstFragmentAtMs: number | null = null;
  const startedAtMs = Date.now();
  // Streamed characters not yet reported to the status line — see the delta
  // branch below.
  let pendingChars = 0;

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
        firstFragmentAtMs ??= Date.now();
        contentStarted = true;
        content += event.content;
        onDelta?.(content);
        // Feeds the status line's live token estimate while the answer is
        // being written — `usage` doesn't arrive until the stream's final
        // chunk, so this is the only signal there is until then. Batched:
        // a store write per delta would re-render the transcript on every
        // token for a number that only reads to the nearest 0.1k anyway.
        if (recordTurnStatusTokens) {
          pendingChars += event.content.length;
          if (pendingChars >= STATUS_CHAR_FLUSH) {
            useTurnStatusStore.getState().noteStreamedChars(sessionId, pendingChars);
            pendingChars = 0;
          }
        }
      } else if (event.type === 'tool_call') {
        firstFragmentAtMs ??= Date.now();
        contentStarted = true;
        toolCalls.push(event.toolCall);
      } else if (event.type === 'usage') {
        usage = {
          promptTokens: event.usage.prompt_tokens,
          completionTokens: event.usage.completion_tokens,
          totalTokens: event.usage.total_tokens,
        };
        const costState = useCostControlStore.getState();
        const targetKey = costTargetKey(target);
        costState.recordUsage({
          occurredAtMs: Date.now(),
          targetKey,
          targetLabel: describeUsageTarget(target),
          sessionId,
          runId: runId ?? null,
          // K25 attribution, captured here because this is the only place a
          // priced call is recorded. Both may be null — a headless/one-shot
          // caller has no session and the app may have no folder open — and
          // null attributes to the "unattributed" bucket rather than to
          // whichever workspace happens to be open when the panel is read.
          ...attributionOf(sessionId),
          usage,
          costUsd: isMeteredTarget(target)
            ? calculateUsageCostUsd(costState.rates[targetKey], usage)
            : 0,
          // Null when `usage` arrived before any content did — an honest
          // "not measured" rather than a 0 that would read as instant.
          timeToFirstTokenMs:
            firstFragmentAtMs === null ? null : firstFragmentAtMs - startedAtMs,
        });
        if (recordUsage) {
          useUsageStore.getState().setUsage(sessionId, usage);
          useUsageHistoryStore.getState().recordUsage(describeUsageTarget(target), usage);
          // Feeds the chat's live status line — a no-op when no turn is
          // registered for `sessionId`. The risk judge opts out via
          // `recordTurnStatusTokens` (see the param's doc comment).
          if (recordTurnStatusTokens) {
            // Exact number supersedes the estimate: the store drops its
            // streamed-character count, and this attempt's unflushed
            // remainder goes with it.
            pendingChars = 0;
            useTurnStatusStore.getState().addTokens(sessionId, usage.totalTokens);
          }
        }
      }
      // 'done' carries no data; the generator simply returns after it.
    }
  } catch (err) {
    streamError = errorMessage(err);
  }

  // An endpoint that never reports `usage` (or a stream that died mid-answer)
  // would otherwise leave the last partial batch off the status line.
  if (recordTurnStatusTokens && pendingChars > 0) {
    useTurnStatusStore.getState().noteStreamedChars(sessionId, pendingChars);
  }

  return { content, toolCalls, streamError, contentStarted, usage };
}
