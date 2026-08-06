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
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { protectToolResult } from './untrustedContent';
import { mutationToolFailureReason } from './workspaceMutation';

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
const MAX_REPORT_CHARS = 8_000;

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
 * Applies the optional per-profile model override (slice 4,
 * `settingsStore.subagentProfileModels`) on top of the parent's own
 * already-resolved `target`, if the user configured one for this specific
 * `profile` — e.g. running every `explore` subagent on a cheap/local model
 * while the parent conversation stays on something stronger. Off by default
 * (`subagentProfileModels` starts empty): when nothing is configured for
 * `profile`, this returns `target` unchanged, so a fresh install behaves
 * exactly like slices 1-3 with zero new surface. Only ever swaps to another
 * PROVIDER target (see that setting's own doc comment for why) — never
 * re-resolves the parent's own target, so a mid-turn manual model switch in
 * the parent still can't split-brain a turn (the design doc's own
 * `resolveTarget`-once invariant, preserved here for the override path too).
 */
function resolveSubagentTarget(target: ResolvedTarget, profile: 'explore' | 'code'): ResolvedTarget {
  const override = useSettingsStore.getState().subagentProfileModels[profile];
  if (!override) return target;
  return { kind: 'provider', providerId: override.providerId, model: override.model };
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
  /** Short (3-6 word) label the model supplied — folded into the child's
   * system prompt so it knows what it's here to do. */
  description: string;
  /** The model's full, self-contained instructions — sent as the child's
   * one user message. The child has no access to the parent conversation
   * beyond this string. */
  prompt: string;
  profile: 'explore' | 'code';
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
export async function runSubagentTask(params: RunSubagentTaskParams): Promise<string> {
  useUsageHistoryStore.getState().recordSubagentTaskStarted();
  const { sessionId, runId, parentCheckpointId, parentSignal, taskId, toolCallId, description, prompt, profile, target, effort, risk, onMutatedPath, onMutationFailure } =
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
  useSubagentStore.getState().start({ sessionId, taskId: storeKey, cancelId: taskId, description, profile });

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
    const roots: PromptWorkspaceRoot[] = useWorkspaceStore.getState().roots;
    const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
    const systemPrompt = buildSubagentSystemPrompt(roots, osLabel, profile, description);
    const tools: ToolDef[] = toolsForProfile(profile);
    const mcpRegistry = emptyMcpRegistry();
    // Resolved once, up front — not re-checked every iteration — for the
    // same "never split-brain mid-run" reason `target` itself is passed down
    // rather than re-resolved (see `RunSubagentTaskParams.target`'s doc
    // comment): a settings change mid-run shouldn't retarget an
    // already-running subagent partway through its own loop.
    const resolvedTarget = resolveSubagentTarget(target, profile);

    for (let iteration = 0; iteration < MAX_SUBAGENT_ITERATIONS; iteration++) {
      await honourPause(taskId, processIdPromise, signal);
      if (signal.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);

      const wireHistory: ChatMessage[] = [{ role: 'system', content: systemPrompt }, ...messages];

      // `recordUsage: false` — see `attemptStream`'s own doc comment: a
      // child attempt's usage must never clobber the PARENT session's
      // context-usage ring. `onDelta` is omitted: nothing renders the
      // child's in-progress streaming content anywhere in this slice.
      const attempt = await attemptStream(resolvedTarget, wireHistory, tools, signal, effort, sessionId, undefined, false);

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
