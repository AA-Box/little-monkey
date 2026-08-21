/**
 * The subagent child loop — `runSubagentTask` is what `turnEngine.ts`'s
 * `executeToolCall` delegates a model-requested `task` tool call to (see
 * that function's `name === 'task'` branch). It drives its own
 * model->tools->model loop, structurally identical in shape to
 * `agentLoop.ts`'s `runAgentTurnBody` but deliberately much smaller: no
 * checkpoints of its own, no failover/vision-switch, no context compaction,
 * no persisted transcript — see the module doc comment in
 * `docs/roadmap/p3-subagents.md` ("The child loop") for the full design.
 *
 * Depth is capped at 1 by construction, not by a runtime guard here: the
 * child's own tool list comes from `toolsForProfile`, which never includes
 * `task` (see that function's doc comment in `tools.ts`) — so there is no
 * tool name a child's model could even call to recurse into a grandchild.
 */
import { detectOsLabel, buildSubagentSystemPrompt, type PromptWorkspaceRoot } from './systemPrompt';
import { toolsForProfile } from './tools';
import {
  attemptStream,
  describeUsageTarget,
  executeToolCall,
  isBlockedInPlanMode,
  isToolCallAllowed,
  CANCELLED_TOOL_RESULT,
  stringifyToolError,
  type ResolvedTarget,
  type RiskAnnotationContext,
} from './turnEngine';
import type { ChatMessage, ToolCall, ToolDef } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { useWorkspaceStore } from '../store/workspaceStore';
import { admitProcess, exitProcess, markProcessRunning } from './processTable';
import { honourPause, forgetPause } from './pauseRegistry';
import { useSubagentStore } from '../store/subagentStore';
import { useSessionStore } from '../store/sessionStore';
import { useSettingsStore } from '../store/settingsStore';
import { routeFromActive, type RoutedTarget } from './targetRouting';
import type { RoutingDecision } from './modelRouting';
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { protectToolResult, unwrapUntrustedContent } from './untrustedContent';
import { mutationToolFailureReason } from './workspaceMutation';
import { customAgentBaseProfile, toolsForCustomAgent, type CustomAgentDef } from './customAgents';
import { useCustomAgentStore } from '../store/customAgentStore';
import { usePermissionStore } from '../store/permissionStore';
import { agentWorktreeClient } from './agentWorktree';

/** Hard cap on model/tool round trips for a single subagent run — smaller
 * than the parent's own `MAX_ITERATIONS` (25, agentLoop.ts) since a
 * subagent's task is meant to be scoped and narrow; a child that still
 * hasn't settled on a final answer after this many round trips is far more
 * likely runaway than legitimately thorough. */
export const MAX_SUBAGENT_ITERATIONS = 15;

/** Caps the final report string returned as the parent's tool result — same
 * "a chatty subagent must never itself blow out the PARENT's context" risk
 * called out in the design doc, mirrored on `mentions.ts`'s
 * `MAX_MENTION_CONTENT_CHARS` precedent for a referenced file's content. */
export const MAX_REPORT_CHARS = 8_000;

function capReport(text: string): string {
  if (text.length <= MAX_REPORT_CHARS) return text;
  return `${text.slice(0, MAX_REPORT_CHARS)}\n\n[Report truncated — subagent's final reply exceeded ${MAX_REPORT_CHARS} characters]`;
}

/** Max length (chars, before an ellipsis) a `SubagentRun.lastActivity` label
 * is truncated to — this is a one-line status shown next to a spinner in
 * `SubagentRow.tsx`, not a place to dump a whole `write_file` body. */
const MAX_ACTIVITY_CHARS = 60;

/** Builds the short `name(args preview)` label `SubagentRow.tsx` shows next
 * to the spinner while a child tool call is in flight — same idea as
 * `MessageList.tsx`'s `ToolCallRow` preview, just single-line and capped
 * shorter since this renders inline rather than in an expandable block. */
function activityLabel(toolCall: ToolCall): string {
  const { name, arguments: rawArguments } = toolCall.function;
  let preview = '';
  if (rawArguments) {
    try {
      const parsed: unknown = JSON.parse(rawArguments);
      if (parsed && typeof parsed === 'object') {
        preview = Object.values(parsed as Record<string, unknown>)
          .map((v) => (typeof v === 'string' ? v : JSON.stringify(v)))
          .join(', ');
      }
    } catch {
      preview = rawArguments;
    }
  }
  const truncated = preview.length > MAX_ACTIVITY_CHARS ? `${preview.slice(0, MAX_ACTIVITY_CHARS)}…` : preview;
  return `${name}(${truncated})`;
}

/** In-flight runs' own AbortControllers, keyed by the run's Rust-facing
 * `taskId` (a `crypto.randomUUID()`, guaranteed unique per concurrent run)
 * — NOT the `storeKey`/`toolCallId` `subagentStore` renders by, whose
 * provider-fallback `call_N` form can collide across two concurrent turns'
 * subagents and would let Stop abort the wrong run. What lets the
 * Background-tasks drawer stop ONE subagent without firing the parent
 * turn's Stop. Entries are removed by `runSubagentTask`'s `finish` helper,
 * so the map only ever holds live runs. */
const activeSubagentControllers = new Map<string, AbortController>();

/** Cancels one in-flight subagent run (the Background-tasks drawer's Stop
 * button). `cancelId` is the run's Rust-facing turn id, surfaced as
 * `SubagentRun.cancelId`. Returns `false` when the run isn't live — already
 * finished, or from a previous app session. The run winds down through the
 * exact same path the parent's Stop uses: its loop sees the aborted signal
 * and finalizes as `'cancelled'` with `CANCELLED_TOOL_RESULT`. */
export function cancelSubagentRun(cancelId: string): boolean {
  const controller = activeSubagentControllers.get(cancelId);
  if (!controller) return false;
  controller.abort();
  return true;
}

/** Pending mid-run user messages per live run, keyed by the run's Rust-facing
 * `taskId` — the exact same keying rule as `activeSubagentControllers` above,
 * and for the same reason: `cancelId` is the only handle the Background-tasks
 * drawer has that is guaranteed unique per concurrent run. Drained at the top
 * of each `runSubagentTask` loop iteration; entries are cleared by the same
 * `finish` that retires the controller, so the map only ever holds live runs. */
const pendingSteerMessages = new Map<string, string[]>();

/** Queues a user message for a live subagent run — the child model sees it as
 * a `user` message at the top of its next loop iteration, after the current
 * iteration's tool results. Returns `false` when the run isn't live (already
 * finished, or restored from a previous app session), mirroring
 * `cancelSubagentRun`'s contract. A message queued during the run's FINAL
 * iteration (the one that produces the report) is dropped with the queue. */
export function steerSubagentRun(cancelId: string, text: string): boolean {
  if (!activeSubagentControllers.has(cancelId)) return false;
  const queue = pendingSteerMessages.get(cancelId);
  if (queue) queue.push(text);
  else pendingSteerMessages.set(cancelId, [text]);
  return true;
}

/** Whether a `write_file`/`edit_file` tool result string represents success
 * rather than the `{"error": ...}` shape `stringifyToolError` produces —
 * used only to decide whether to report a mutated path via
 * `RunSubagentTaskParams.onMutatedPath`. Structurally identical to
 * `agentLoop.ts`'s `isSuccessfulMutationResult`, kept as its own tiny copy
 * here (rather than a shared import) so this module never depends on
 * `agentLoop.ts` — see `turnEngine.ts`'s module doc comment for why that
 * dependency direction must stay one-way (subagents/verify/Plan-Act consume
 * `turnEngine.ts`'s primitives without depending on `agentLoop.ts`'s own
 * orchestration layer). */
function isSuccessfulMutationResult(resultContent: string): boolean {
  try {
    const parsed: unknown = JSON.parse(resultContent);
    return !(parsed && typeof parsed === 'object' && 'error' in parsed);
  } catch {
    return true;
  }
}

/** Extracts the `path` argument from a `write_file`/`edit_file` tool call —
 * same tiny-copy reasoning as `isSuccessfulMutationResult` above, mirroring
 * `agentLoop.ts`'s `toolCallPathArg`. */
function toolCallPathArg(toolCall: ToolCall): string | null {
  try {
    const parsed: unknown = JSON.parse(toolCall.function.arguments || '{}');
    const path = (parsed as { path?: unknown } | null)?.path;
    return typeof path === 'string' ? path : null;
  } catch {
    return null;
  }
}

/** No subagent profile offered in this slice ever includes an MCP tool (see
 * `toolsForProfile`), so the child never needs a real per-turn MCP registry
 * — an always-empty one is enough to satisfy `executeToolCall`'s signature
 * (any `mcp__`-prefixed call would just fail to resolve, which can't happen
 * since none is ever offered). Built fresh per run rather than shared as a
 * module-level constant purely so nothing outside this module could ever
 * accidentally mutate a shared instance — it costs nothing since it's never
 * populated either way. */
function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

/**
 * Decides which model runs this subagent: the explicit per-profile override if
 * the user set one, otherwise whatever K9's dispatch policy chooses, otherwise
 * the parent's own target.
 *
 * That order is the whole rule. `settingsStore.subagentProfileModels` is a
 * target the user pinned to this profile by hand — e.g. every `explore`
 * subagent on a cheap or local model while the conversation stays on something
 * stronger — and a policy is a rule about work nobody pinned. A policy that
 * quietly overrode a hand-picked model would make the setting a suggestion.
 *
 * Routing here at all is new: subagents used to be the one dispatch surface no
 * policy could reach, because this module cannot import `agentLoop.ts`
 * (`turnEngine.ts` imports this one, so the edge would close a cycle) and
 * target resolution lived there. It now lives in `targetRouting.ts`, which has
 * no such edge — the lift is what made the two subagent task classes possible.
 *
 * Never re-resolves the parent's own target, exactly as before: a mid-run
 * settings or model change must not retarget a subagent partway through its own
 * loop, which is the `resolveTarget`-once invariant this call site has always
 * held.
 *
 * The routing decision is returned rather than only its target, so the caller
 * can record *why* this subagent ran where it did.
 */
/** How a `profile` string resolves before the child loop starts — exported
 * (with `resolveSubagentProfile`) for the DOM-free logic tests. A custom
 * def's `base` is the built-in profile its ROUTING rides (`code` iff the def
 * grants a mutating tool): per-profile model pins and dispatch policies only
 * know the two built-in task classes, and a custom agent is one of those two
 * kinds of work as far as model choice is concerned. */
export type ResolvedSubagentProfile =
  | { kind: 'builtin'; profile: 'explore' | 'code' }
  | { kind: 'custom'; def: CustomAgentDef; base: 'explore' | 'code' }
  | { kind: 'unknown'; known: string[] };

export function resolveSubagentProfile(
  profile: string,
  defs: Record<string, CustomAgentDef>,
): ResolvedSubagentProfile {
  if (profile === 'explore' || profile === 'code') return { kind: 'builtin', profile };
  const def = defs[profile];
  if (def) return { kind: 'custom', def, base: customAgentBaseProfile(def) };
  return { kind: 'unknown', known: ['explore', 'code', ...Object.keys(defs).sort()] };
}

function resolveSubagentTarget(
  target: ResolvedTarget,
  profile: 'explore' | 'code',
): RoutedTarget {
  const override = useSettingsStore.getState().subagentProfileModels[profile];
  if (override) {
    const pinned: ResolvedTarget = {
      kind: 'provider',
      providerId: override.providerId,
      model: override.model,
    };
    return { target: pinned, decision: pinnedDecision(profile, override), sequence: [pinned] };
  }
  return routeFromActive(target, {
    taskClass: profile === 'explore' ? 'subagent_explore' : 'subagent_code',
    // A subagent is never handed an image (`runSubagentTask` builds its history
    // from a text description) and is always offered tools by
    // `toolsForProfile`, so both constraints are facts about this surface
    // rather than per-run guesses.
    requiresVision: false,
    requiresTools: true,
  });
}

/** The "no policy ran, the user had already chosen" decision, in the same shape
 * a real one takes so the recorder below has one code path rather than two. */
function pinnedDecision(
  profile: 'explore' | 'code',
  override: { providerId: string; model: string },
): RoutingDecision {
  return {
    policyId: null,
    policyName: null,
    taskClass: profile === 'explore' ? 'subagent_explore' : 'subagent_code',
    chosenKey: null,
    sequence: [],
    rejected: [],
    reason: `Pinned by the ${profile} subagent model setting (${override.providerId} · ${override.model}), so no dispatch policy was consulted.`,
    changedFromActive: true,
  };
}

/** Everything `runSubagentTask` needs to drive one subagent run — see
 * `turnEngine.ts`'s `executeToolCall` `name === 'task'` branch, the sole
 * caller in this slice, for how each field is derived from the parent
 * turn's own state. */
export interface RunSubagentTaskParams {
  /** The parent turn's session id — threaded through to `attemptStream`
   * purely because that function requires one for its `usage` event
   * plumbing; passed with `recordUsage: false` below, so it is never
   * actually written anywhere (see that param's own doc comment). */
  sessionId: string;
  /** Durable parent run used for permission and cancellation audit. */
  runId?: string;
  /** The PARENT turn's checkpoint id (or `null` if the parent turn has none
   * — e.g. bypass mode with nothing mutating yet). Passed straight through
   * to every child tool call unchanged, so any file a `code`-profile child
   * (slice 3) mutates lands in the PARENT's checkpoint manifest and is
   * revertable via the existing CheckpointRow — `explore`-profile children
   * in this slice never call a checkpoint-eligible tool at all, but the
   * plumbing is already correct for when they do. */
  parentCheckpointId: string | null;
  /** Called once with this subagent's K9 dispatch decision — see
   * `turnEngine.ts`'s `SubagentContext.onRoutingDecision` for why it is a
   * callback rather than a recorder handle. */
  onRoutingDecision?: (decision: RoutingDecision) => void;
  /** The parent turn's own `AbortSignal` — Stop in the parent pane must
   * cancel the whole subagent tree, exactly like it cancels any other
   * in-flight tool call the parent turn is waiting on. */
  parentSignal?: AbortSignal;
  /** This subagent run's OWN unique id (`crypto.randomUUID()`, generated by
   * the `executeToolCall` call site) — used as the child's own `turnId` for
   * every tool call it makes, scoping Rust's per-turn `tool_cancel`/
   * permission-`pending` maps to this run specifically rather than the
   * parent's. Also the natural key for a future `subagentStore` (slice 2);
   * unused for that purpose in this slice, but threaded through now so that
   * addition doesn't need to change this signature. */
  taskId: string;
  /** The originating `task` tool_call's own `ToolCall.id` — deliberately a
   * SEPARATE id from `taskId` above, and used for a different purpose:
   * `taskId` scopes Rust-side cancellation/permission state and must stay a
   * `crypto.randomUUID()` (a `call_0`-style provider fallback id could
   * collide across two concurrent turns' subagents), while THIS id is only
   * ever used as the `subagentStore`/`ChatSession.subagentRuns` key —
   * `MessageList.tsx`'s `buildTimeline` has this id on hand (it's right
   * there in the transcript) but has no way to learn `taskId`, which is
   * generated fresh inside `executeToolCall` and never written into the
   * transcript. Optional (falls back to `taskId`) purely so existing tests
   * that construct `RunSubagentTaskParams` by hand don't all need updating —
   * every real caller (`turnEngine.ts`) always supplies it. */
  toolCallId?: string;
  /** Shared id linking the parallel `task` calls of one assistant round —
   * `undefined` for a lone call. See `SubagentRun.groupId`; threaded into
   * `subagentStore.start` and the finish-time `SubagentRunMeta` snapshot so
   * the Background-tasks drawer can group the round's runs into one card. */
  groupId?: string;
  /** Set when this run is one agent of a `workflow` tool call — see
   * `SubagentRun.workflowRunId`. Threaded into `subagentStore.start` and the
   * finish-time meta snapshot, exactly like `groupId`. */
  workflowRunId?: string;
  /** Short (3-6 word) label the model supplied — folded into the child's
   * system prompt so it knows what it's here to do. */
  description: string;
  /** The model's full, self-contained instructions — sent as the child's
   * one user message. The child has no access to the parent conversation
   * beyond this string. */
  prompt: string;
  /** `'explore'`, `'code'`, or a loaded custom agent's name (see
   * `customAgents.ts`). An unknown name resolves to a tool-error result
   * naming the known profiles — never a silent fallback to a built-in. */
  profile: string;
  /** `'worktree'` runs this (code-class) agent against a fresh managed git
   * worktree of the primary root — see `runSubagentTask`'s isolation
   * wrapper. Only meaningful with a mutating profile; anything else is a
   * tool error, never a silent fallthrough to the shared checkout. */
  isolation?: 'worktree';
  /** INTERNAL (set only by the isolation wrapper): fail the run with this
   * error immediately after registering it, so preflight failures (bad
   * isolation/profile combo, worktree creation failure) still surface as a
   * truthful errored run in the UI instead of a store-less orphan result. */
  preflightError?: string;
  /** INTERNAL (set only by the isolation wrapper): the managed worktree path
   * threaded into every child tool call as `executeToolCall`'s per-call
   * root override — never global state, so concurrent isolated agents can't
   * race. */
  workspaceRootOverride?: string;
  /** INTERNAL (set only by the isolation wrapper): what the child's system
   * prompt names as its workspace, in place of the real roots — an isolated
   * child must be told it works in the worktree. */
  promptWorkspaceRoots?: PromptWorkspaceRoot[];
  /** THIS turn's already-resolved active target — passed down rather than
   * re-resolved, so a mid-turn manual model switch in the parent can never
   * split the parent and child across different targets mid-turn. */
  target: ResolvedTarget;
  effort?: string;
  /** The PARENT turn's own risk-annotation context (see
   * `turnEngine.ts`'s `SubagentContext.risk` doc comment) — threaded
   * straight through to every `executeToolCall` this run makes, so a
   * `code`-profile child's write_file/edit_file/run_shell calls get the same
   * advisory risk classification (and share the same per-turn cache) the
   * parent's own equivalent calls would, instead of silently skipping
   * classification just because the mutation happened inside a subagent.
   * `undefined` when the parent turn never built one (risk annotations off). */
  risk?: RiskAnnotationContext;
  /** Called once per path this run's `code`-profile child successfully
   * `write_file`/`edit_file`s — see `turnEngine.ts`'s
   * `SubagentContext.onMutatedPath` doc comment for why this exists: without
   * it, the parent's own `mutatedFiles` tracking never learns about a
   * subagent's mutations, so `runVerificationPhase` silently never fires for
   * a turn where every mutation happened inside a delegated `task` call.
   * `undefined` for a caller that doesn't track mutated files at all. */
  onMutatedPath?: (path: string) => void;
  /** Reports a failed `write_file`/`edit_file` attempt to the parent mutation
   * contract. The originating tool-call id gives path-less failures a stable,
   * unique key; a later success for the same concrete path clears the failure
   * through `onMutatedPath`. */
  onMutationFailure?: (
    path: string | null,
    reason: string,
    toolCallId: string,
  ) => void;
  onStructuredResult?: (result: RunSubagentTaskResult) => void;
  /** Node-level capability ceiling, intersected with the built-in profile and
   * the frozen task policy before tools are offered or dispatched. */
  capabilities?: readonly string[];
}

export interface RunSubagentTaskResult {
  report: string;
  outcome: "done" | "error" | "cancelled";
  changedFiles: string[];
  worktree?: { id: string; path: string; branch: string; baseRevision: string; diffDigest: string };
  usage?: { modelCalls: number; toolCalls: number; inputTokens: number; outputTokens: number; costMicros: number };
}

/**
 * Runs one subagent's model->tools->model loop to completion (or the
 * `MAX_SUBAGENT_ITERATIONS` cap) and returns the string to use as the
 * PARENT's `task` tool result: either the child's final assistant reply
 * (capped by `capReport`), or a `stringifyToolError`-shaped `{"error":...}`
 * payload on failure/cap-exceeded/cancellation, using the exact same
 * conventions `turnEngine.ts`'s `executeToolCall` uses for every other tool
 * — so the parent model can see what went wrong and try to recover, and so
 * the transcript-validity invariant (every `tool_calls` entry gets a
 * matching `tool` result) holds even when this whole function is what's
 * supposed to produce that result.
 *
 * The ENTIRE body runs inside a try/catch that returns
 * `stringifyToolError(err)` for anything unexpected — this function must
 * never let an exception propagate to its caller (`executeToolCall`, which
 * wraps its own call to this in another try/catch as defense in depth, but
 * the invariant is owned here first).
 */
/** True when a loop result is an `{"error":...}` payload rather than a
 * report — same reading `workflow.ts`'s `resultIsError` applies, duplicated
 * as a tiny local (that module imports this one). */
function isErrorResult(result: string): boolean {
  try {
    const parsed: unknown = JSON.parse(unwrapUntrustedContent(result));
    return typeof parsed === 'object' && parsed !== null && 'error' in parsed;
  } catch {
    return false;
  }
}

/**
 * Public entry point — plain runs go straight to the loop; a
 * `isolation: 'worktree'` run wraps it in the worktree lifecycle:
 * validate (code-class profiles only) → create the managed worktree →
 * run the loop with every tool call rooted there → then either remove the
 * untouched worktree, or keep it and record `{path, diffstat}` on the run's
 * persisted meta for the SubagentRow footer's Apply/Discard. A kept
 * worktree survives EVERY non-clean outcome — cancellation and errors
 * included — because deleting uncommitted agent work on a Stop click is the
 * one unforgivable behavior here. The epilogue itself never throws: a
 * worktree bookkeeping failure must not corrupt the tool result.
 */
export async function runSubagentTask(params: RunSubagentTaskParams): Promise<string> {
  if (params.isolation !== 'worktree') {
    const report = await runSubagentTaskLoop(params);
    const live = useSubagentStore.getState().runs[params.toolCallId ?? params.taskId];
    params.onStructuredResult?.({ report, outcome: structuredOutcome(report), changedFiles: [], usage: live?.usage ? { modelCalls: 1, toolCalls: live.toolCallCount, inputTokens: live.usage.promptTokens, outputTokens: live.usage.completionTokens, costMicros: 0 } : undefined });
    return report;
  }

  const resolved = resolveSubagentProfile(params.profile, useCustomAgentStore.getState().defs);
  const base = resolved.kind === 'builtin' ? resolved.profile : resolved.kind === 'custom' ? resolved.base : null;
  if (base !== 'code') {
    return runSubagentTaskLoop({
      ...params,
      preflightError:
        base === null
          ? `Unknown agent profile "${params.profile}" for a worktree-isolated task. Known profiles: ${resolved.kind === 'unknown' ? resolved.known.join(', ') : ''}.`
          : `Worktree isolation requires a mutating (code-class) profile — "${params.profile}" is read-only. Drop "isolation" or use a code-class profile.`,
    });
  }

  let created: { path: string; branch: string };
  try {
    created = await agentWorktreeClient.create();
  } catch (err) {
    return runSubagentTaskLoop({
      ...params,
      preflightError: `Failed to create an isolated worktree: ${err instanceof Error ? err.message : String(err)}`,
    });
  }

  const result = await runSubagentTaskLoop({
    ...params,
    // The isolated child's changes never join the parent turn's checkpoint:
    // they live in the worktree, and Apply/Discard IS their revert story —
    // a checkpoint manifest full of paths inside a possibly-deleted temp
    // worktree would promise a revert it can't keep.
    parentCheckpointId: null,
    workspaceRootOverride: created.path,
    promptWorkspaceRoots: [{ path: created.path, label: 'worktree', is_primary: true }],
  });

  try {
    const st = await agentWorktreeClient.status(created.path);
    if (!st.dirty) {
      // Nothing was produced — an empty worktree holds no agent work, so
      // removing it is safe on every outcome, cancellation included.
      await agentWorktreeClient.remove(created.path, false);
      params.onStructuredResult?.({ report: result, outcome: structuredOutcome(result), changedFiles: [], worktree: { id: created.branch, path: created.path, branch: created.branch, baseRevision: st.base_revision, diffDigest: st.patch_digest } });
      return result;
    }
    useSessionStore
      .getState()
      .setSubagentWorktree(params.sessionId, params.toolCallId ?? params.taskId, {
        path: created.path,
        diffstat: st.diffstat,
        status: 'kept',
      });
    params.onStructuredResult?.({ report: result, outcome: structuredOutcome(result), changedFiles: st.changed_files, worktree: { id: created.branch, path: created.path, branch: created.branch, baseRevision: st.base_revision, diffDigest: st.patch_digest } });
    return isErrorResult(result)
      ? result
      : `${result}\n\n[Changes were left in an isolated worktree at ${created.path} — NOT applied to the workspace. The user can apply or discard them from this agent's row.]\nDiffstat:\n${st.diffstat}`;
  } catch {
    return result;
  }
}

function structuredOutcome(result: string): RunSubagentTaskResult["outcome"] {
  if (/cancel/i.test(result) && isErrorResult(result)) return "cancelled";
  return isErrorResult(result) ? "error" : "done";
}

export async function runSubagentTaskStructured(params: RunSubagentTaskParams): Promise<RunSubagentTaskResult> {
  let structured: RunSubagentTaskResult | undefined;
  const report = await runSubagentTask({ ...params, onStructuredResult: (result) => { structured = result; params.onStructuredResult?.(result); } });
  return structured ?? { report, outcome: structuredOutcome(report), changedFiles: [] };
}

async function runSubagentTaskLoop(params: RunSubagentTaskParams): Promise<string> {
  useUsageHistoryStore.getState().recordSubagentTaskStarted();
  const { sessionId, runId, parentCheckpointId, parentSignal, taskId, toolCallId, groupId, workflowRunId, description, prompt, profile, target, effort, risk, onMutatedPath, onMutationFailure, capabilities } =
    params;

  // The key `subagentStore`/`ChatSession.subagentRuns` are updated under —
  // see `RunSubagentTaskParams.toolCallId`'s doc comment for why this must
  // NOT be `taskId` itself (that id is Rust-turn-scoped, not transcript-
  // correlatable).
  const storeKey = toolCallId ?? taskId;

  // The child's own local transcript — never written into `sessionStore`'s
  // `messages` (the array `agentLoop.ts` actually feeds into `wireHistory`
  // for the NEXT parent turn) — only ever into `subagentStore` (live) and
  // `ChatSession.subagentRuns` (persisted) below, both of which are purely
  // UI-side and never read when assembling a turn's wire payload. Declared
  // here (before the try block) so `finish` can persist whatever the child
  // produced even if something throws before the loop below completes.
  let messages: ChatMessage[] = [{ role: 'user', content: prompt }];

  // Registered immediately (before any streaming happens) so `SubagentRow`
  // can render a spinner for this task the instant the parent turn
  // dispatches it — not just once the first `attemptStream` call settles.
  useSubagentStore.getState().start({ sessionId, taskId: storeKey, cancelId: taskId, groupId, workflowRunId, description, profile });

  // Projected onto the unified process table as a child of the turn that
  // dispatched it. `taskId` (the cancel id) is the surface identifier rather
  // than `storeKey`, because `storeKey` is the originating `ToolCall.id` and a
  // provider fallback id like `call_0` is not unique. The parent is named by its
  // turn id, which is all this function is given; resolution happens in Rust.
  // Fail-soft — see `processTable.ts`.
  const processIdPromise = admitProcess({
    kind: 'subagent',
    externalId: taskId,
    parentExternalId: runId ?? null,
    parentKind: 'chat_turn',
    profile,
  }).then(async (id) => {
    if (id) await markProcessRunning(id);
    return id;
  });

  // This run's OWN signal: aborted by the parent turn's Stop (relayed from
  // `parentSignal`) OR by `cancelSubagentRun` targeting just this run from
  // the Background-tasks drawer. Everything below checks/passes `signal`,
  // never `parentSignal` directly, so both cancellation sources flow
  // through the same single wind-down path.
  const ownController = new AbortController();
  const signal = ownController.signal;
  if (parentSignal?.aborted) ownController.abort();
  else parentSignal?.addEventListener('abort', () => ownController.abort(), { once: true });
  activeSubagentControllers.set(taskId, ownController);

  /** Marks this run terminal in both the live store and the persisted
   * session field, then returns `result` unchanged — the single exit point
   * every return statement below routes through, so every outcome (report,
   * error, cancellation, iteration-cap) reliably finalizes both places
   * exactly once. Also retires the cancellation handle: a Stop click after
   * this point is a no-op rather than an abort of a reused controller.
   * The live store entry's stats are snapshotted into
   * `ChatSession.subagentRunMeta` alongside the transcript, so the
   * Background-tasks drawer and `SubagentRow` keep tokens/timing/status
   * after a restart wipes the transient store. */
  const finish = (status: 'done' | 'error' | 'cancelled', result: string): string => {
    activeSubagentControllers.delete(taskId);
    pendingSteerMessages.delete(taskId);
    forgetPause(taskId);
    const live = useSubagentStore.getState().runs[storeKey];
    useSubagentStore.getState().finish(storeKey, status);
    useSessionStore.getState().setSubagentRun(
      sessionId,
      storeKey,
      messages,
      live
        ? {
            status,
            groupId: live.groupId,
            workflowRunId: live.workflowRunId,
            description: live.description,
            profile: live.profile,
            startedAt: live.startedAt,
            finishedAt: Date.now(),
            toolCallCount: live.toolCallCount,
            usage: live.usage,
          }
        : undefined,
    );
    // Not awaited: `finish` is synchronous and is the single exit point every
    // return routes through, so blocking it on IPC would change the loop's
    // shape. The projection is best-effort by design.
    void processIdPromise.then((id) => {
      if (!id) return;
      const outcome =
        status === 'done'
          ? { status: 'succeeded' as const, reason: null }
          : status === 'cancelled'
            ? { status: 'cancelled' as const, reason: 'stopped' }
            : { status: 'failed' as const, reason: result.slice(0, 500) };
      return exitProcess(id, outcome.status, outcome.reason);
    });
    return result;
  };

  try {
    // A preflight failure from the isolation wrapper — the run was
    // registered above so the UI shows a truthful errored row, and fails
    // here before any model streaming.
    if (params.preflightError) {
      return finish('error', stringifyToolError(new Error(params.preflightError)));
    }
    // Resolved from the store ONCE, before the loop — a def file edited (or
    // deleted) mid-run must not change an already-running agent's tools.
    const resolvedProfile = resolveSubagentProfile(profile, useCustomAgentStore.getState().defs);
    if (resolvedProfile.kind === 'unknown') {
      return finish(
        'error',
        stringifyToolError(
          new Error(`Unknown agent profile "${profile}". Known profiles: ${resolvedProfile.known.join(', ')}.`)
        )
      );
    }
    const baseProfile = resolvedProfile.kind === 'custom' ? resolvedProfile.base : resolvedProfile.profile;
    // Plan Mode refusal, at DISPATCH rather than per child tool call: a
    // `code`-class agent exists to make changes, and every change it tried
    // would be refused anyway (`executeToolCall`'s Plan-Mode backstop + the
    // Rust mode gate) — refusing the whole dispatch with one actionable
    // error beats burning a child loop on doomed calls. Snapshotted once,
    // like every other per-run resolution here: the user approving the plan
    // mid-run must not retroactively rewire an already-running child.
    const planMode = usePermissionStore.getState().mode === 'plan';
    if (planMode && baseProfile === 'code') {
      return finish(
        'error',
        stringifyToolError(
          new Error(
            `Blocked: Little Monkey is in Plan Mode, and the "${profile}" agent profile can make changes. Use profile "explore" (or a read-only custom agent) to investigate, or ask the user to approve the plan first.`
          )
        )
      );
    }
    const roots: PromptWorkspaceRoot[] = params.promptWorkspaceRoots ?? useWorkspaceStore.getState().roots;
    const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
    const systemPrompt = buildSubagentSystemPrompt(
      roots,
      osLabel,
      baseProfile,
      description,
      resolvedProfile.kind === 'custom' ? resolvedProfile.def : undefined,
    );
    // A custom def's list is re-intersected with the ceiling inside
    // `toolsForCustomAgent` (defense in depth on top of load-time
    // validation); `isToolCallAllowed` below then enforces exactly this list
    // per call, so a granted-but-hallucinated name never dispatches either.
    const profileTools: ToolDef[] =
      resolvedProfile.kind === 'custom' ? toolsForCustomAgent(resolvedProfile.def) : toolsForProfile(resolvedProfile.profile);
    const capabilitySet = new Set(capabilities ?? ['read', 'mutate', 'verify', 'network', 'delegate']);
    const readOnlyTools = new Set(['read_file', 'list_dir', 'glob', 'grep', 'search_docs', 'read_skill_resource']);
    const mutationTools = new Set(['write_file', 'edit_file', 'remember', 'manage_skill_learning']);
    const verificationTools = new Set(['run_shell', 'shell_output', 'shell_kill']);
    const networkTools = new Set(['web_fetch', 'web_search', 'device_action']);
    const delegationTools = new Set(['task', 'spawn_task', 'workflow']);
    const policyTools = profileTools.filter((tool) => {
      const name = tool.function.name;
      if (readOnlyTools.has(name)) return capabilitySet.has('read');
      if (mutationTools.has(name)) return capabilitySet.has('mutate');
      if (verificationTools.has(name)) return capabilitySet.has('verify') || capabilitySet.has('mutate');
      if (networkTools.has(name)) return capabilitySet.has('network');
      if (delegationTools.has(name)) return capabilitySet.has('delegate');
      return capabilitySet.has('read');
    });
    // In Plan Mode an `explore`-class agent still dispatches, but any name
    // Plan Mode refuses (a read-only custom agent may hold web tools, which
    // are permission-gated) is dropped from the child's OFFER too — same
    // fail-closed double layer the parent's own `toolsForMode` applies.
    const tools: ToolDef[] = planMode ? policyTools.filter((tool) => !isBlockedInPlanMode(tool.function.name)) : policyTools;
    // The def's own effort wins over the inherited turn effort — that's what
    // declaring `effort` in the definition file is FOR; a caller has no way
    // to signal "explicitly override the def" separately from "inherited".
    const childEffort = resolvedProfile.kind === 'custom' ? (resolvedProfile.def.effort ?? effort) : effort;
    const mcpRegistry = emptyMcpRegistry();
    // Resolved once, up front — not re-checked every iteration — for the
    // same "never split-brain mid-run" reason `target` itself is passed down
    // rather than re-resolved (see `RunSubagentTaskParams.target`'s doc
    // comment): a settings change mid-run shouldn't retarget an
    // already-running subagent partway through its own loop.
    const routed = resolveSubagentTarget(target, baseProfile);
    const resolvedTarget = routed.target;
    // Onto the parent's run: a subagent has none of its own, and the decision
    // is about work the parent asked for.
    params.onRoutingDecision?.(routed.decision);

    for (let iteration = 0; iteration < MAX_SUBAGENT_ITERATIONS; iteration++) {
      await honourPause(taskId, processIdPromise, signal);
      if (signal.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);

      // Drain any user messages queued by `steerSubagentRun` since the last
      // iteration — appended in queue order as plain `user` messages, so the
      // child model sees them right after the tool results it just produced.
      const steers = pendingSteerMessages.get(taskId);
      if (steers && steers.length > 0) {
        pendingSteerMessages.delete(taskId);
        for (const text of steers) {
          const steerMessage: ChatMessage = { role: 'user', content: text };
          messages = [...messages, steerMessage];
          useSubagentStore.getState().appendMessage(storeKey, steerMessage);
        }
      }

      const wireHistory: ChatMessage[] = [{ role: 'system', content: systemPrompt }, ...messages];

      // `recordUsage: false` — see `attemptStream`'s own doc comment: a
      // child attempt's usage must never clobber the PARENT session's
      // context-usage ring. `onDelta` is omitted: nothing renders the
      // child's in-progress streaming content anywhere in this slice.
      const attempt = await attemptStream(resolvedTarget, wireHistory, tools, signal, childEffort, sessionId, undefined, false);

      if (attempt.usage) {
        useSubagentStore.getState().accumulateUsage(storeKey, attempt.usage);
        useUsageHistoryStore.getState().recordUsage(describeUsageTarget(resolvedTarget), attempt.usage);
      }

      // An abort (parent Stop or this run's own Stop button) surfaces as a
      // stream exception, which would otherwise mislabel a deliberate
      // cancellation as 'error'/"Failed". Narrowed to the streamError case
      // so an abort that arrives AFTER a fully-streamed final reply still
      // delivers the report through the branches below.
      if (signal.aborted && attempt.streamError !== null) {
        return finish('cancelled', CANCELLED_TOOL_RESULT);
      }

      if (attempt.streamError !== null) {
        return finish('error', stringifyToolError(new Error(attempt.streamError)));
      }

      if (attempt.toolCalls.length === 0) {
        if (signal.aborted && attempt.content.length === 0) return finish('cancelled', CANCELLED_TOOL_RESULT);
        const finalMessage: ChatMessage = { role: 'assistant', content: attempt.content };
        messages = [...messages, finalMessage];
        useSubagentStore.getState().appendMessage(storeKey, finalMessage);
        return finish('done', capReport(attempt.content.trim() || '(subagent finished with no report)'));
      }

      const assistantMessage: ChatMessage = { role: 'assistant', content: attempt.content, tool_calls: attempt.toolCalls };
      messages = [...messages, assistantMessage];
      useSubagentStore.getState().appendMessage(storeKey, assistantMessage);

      for (const toolCall of attempt.toolCalls) {
        // Once Stop has fired, remaining calls are not executed — but every
        // one still gets a (cancelled) result message, same invariant
        // `runAgentTurnBody`'s own loop upholds for the parent's tool calls.
        const aborted = signal.aborted;
        if (!aborted) {
          useSubagentStore.getState().recordToolCall(storeKey, activityLabel(toolCall));
        }
        // Reject (without executing) any call whose name isn't actually
        // among the tools THIS profile was offered — the same
        // `isToolCallAllowed` gate `agentLoop.ts`'s parent loop applies to
        // its own tool calls, reused here rather than a parallel check.
        // Without this, an `explore`-profile child driven by a
        // local/quantized model that doesn't strictly respect the offered
        // tool schema could still have a hallucinated `write_file`/
        // `edit_file`/`run_shell` call actually dispatched and executed —
        // `toolsForProfile` only shapes what's *offered*; this is the
        // enforcement point that makes it an actual authorization boundary.
        const allowed = isToolCallAllowed(toolCall, tools);
        const resultContent = aborted
          ? CANCELLED_TOOL_RESULT
          : !allowed
            ? stringifyToolError(
                new Error(`Tool "${toolCall.function.name}" was not offered to this ${profile}-profile subagent and was not executed.`)
              )
            : // `parentCheckpointId` + `taskId` is the crux pairing that makes
              // `code`-profile subagents safe (see `RunSubagentTaskParams`'s
              // doc comments on those two fields): the PARENT's checkpoint id
              // so any write/edit lands in the parent turn's own checkpoint
              // manifest, but this run's OWN turn id so Rust's per-turn
              // `tool_cancel`/permission-`pending` maps scope cancellation and
              // prompts to just this subagent — never the parent's own
              // in-flight tool call, and never some other concurrent turn's.
              // `risk` is the parent turn's own risk-annotation context (see
              // `RunSubagentTaskParams.risk`'s doc comment), so a `code`-
              // profile child's mutations get classified exactly like the
              // parent's own. `description` is threaded through as
              // `agentLabel` (slice 3) so a `code`-profile child's write_file/
              // edit_file/run_shell permission prompt is attributed to THIS
              // subagent (see `turnEngine.ts`'s `executeToolCall` injection
              // site and `permissions.rs`'s `PermissionRequestPayload.
              // agent_label`) — harmless for `explore` children too, since
              // none of their tools are permission-gated mutations that
              // read it.
              await executeToolCall(
                toolCall,
                parentCheckpointId,
                runId ?? taskId,
                mcpRegistry,
                signal,
                risk,
                undefined,
                undefined,
                description,
                undefined,
                undefined,
                params.workspaceRootOverride,
              );
        const toolMessage: ChatMessage = {
          role: 'tool',
          tool_call_id: toolCall.id,
          content: allowed
            ? protectToolResult(toolCall.function.name, resultContent, mcpRegistry.has(toolCall.function.name))
            : resultContent,
        };
        messages = [...messages, toolMessage];
        useSubagentStore.getState().appendMessage(storeKey, toolMessage);

        // Surface a successful `code`-profile mutation to the PARENT's own
        // `mutatedFiles` tracking (see `RunSubagentTaskParams.onMutatedPath`'s
        // doc comment) — without this, `runVerificationPhase` never fires for
        // a turn where every mutation happened inside a delegated `task`
        // call, since the parent round's own `toolCalls` only ever contains
        // the single `task` entry.
        if (
          !aborted
          && allowed
          && (toolCall.function.name === 'write_file' || toolCall.function.name === 'edit_file')
        ) {
          const path = toolCallPathArg(toolCall);
          if (isSuccessfulMutationResult(resultContent)) {
            if (path) onMutatedPath?.(path);
          } else {
            onMutationFailure?.(
              path,
              mutationToolFailureReason(resultContent)
                ?? 'The file-mutation tool returned an error.',
              toolCall.id,
            );
          }
        }
      }

      await honourPause(taskId, processIdPromise, signal);
      if (signal.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);
      // Loop again: the child model gets its own tool results appended.
    }

    return finish(
      'error',
      stringifyToolError(
        new Error(`Subagent stopped after reaching the safety limit of ${MAX_SUBAGENT_ITERATIONS} tool-calling iterations without a final answer.`)
      )
    );
  } catch (err) {
    return finish('error', stringifyToolError(err));
  }
}
