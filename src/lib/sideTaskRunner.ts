/**
 * The side-task engine — drives one side task's own, independent
 * model->tools->model loop (see ROADMAP.md's "Side Tasks" item, Phase 1).
 * Structurally close to `subagent.ts`'s `runSubagentTask` (same restricted
 * `toolsForProfile` tool sets, same `executeToolCall`/`attemptStream`
 * primitives from `turnEngine.ts`), but deliberately NOT the same thing:
 *
 * - A subagent is spawned BY THE MODEL mid-turn via the `task` tool call and
 *   reports back into the SAME turn that spawned it; a side task is started
 *   BY THE USER from something already on screen (a chat message, a file
 *   selection, terminal output, browser evidence, an MCP result) and never
 *   blocks or is blocked by the main chat turn — it has its own lifecycle
 *   (queued/running/paused/completed/error/cancelled, see
 *   `sideTaskStore.ts`) that outlives any single parent turn.
 * - A subagent's only output path is its return value, fed straight back
 *   into the parent turn's own transcript. A side task's output NEVER
 *   touches any chat session's transcript on its own — see
 *   `promoteSideTask` below, the only path that appends anything to
 *   `sessionStore`, and it only runs on an explicit user click.
 * - Isolation: every tool call this module executes goes through the exact
 *   same `turnEngine.ts` `executeToolCall`/Rust `request_permission` gate as
 *   the main chat and every subagent — nothing new was added to
 *   `src-tauri/src/permissions.rs` for this feature because the permission
 *   system already scopes both "allow for this run" grants
 *   (`PermissionState.run_allow`, keyed by turn id) and Stop-button
 *   cancellation (`tools_cancel_running`, also keyed by turn id) per turn —
 *   see `startSideTask` below, which mints a brand-new `crypto.randomUUID()`
 *   turn id per attempt specifically so a side task's approvals/denials can
 *   never leak into (or be satisfied by) the main chat turn's own grants, or
 *   another side task's.
 */
import {
  detectOsLabel,
  type PromptWorkspaceRoot,
} from './systemPrompt';
import { toolsForProfile } from './tools';
import { extractArtifacts } from './artifacts';
import { resolveTarget } from './agentLoop';
import {
  attemptStream,
  describeUsageTarget,
  executeToolCall,
  isToolCallAllowed,
  CANCELLED_TOOL_RESULT,
  stringifyToolError,
  type ResolvedTarget,
} from './turnEngine';
import type { ChatMessage, ToolCall, ToolDef } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { useWorkspaceStore } from '../store/workspaceStore';
import {
  useSideTaskStore,
  type SideTaskProfile,
  type SideTaskSource,
  type SideTaskArtifact,
  type SideTaskToolOutcome,
} from '../store/sideTaskStore';
import { useSessionStore } from '../store/sessionStore';
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { protectToolResult } from './untrustedContent';
import { admitProcess, exitProcess, markProcessRunning } from './processTable';
import { errorMessage } from "./errors";

/** Hard cap on model/tool round trips for one side task attempt — same
 * order of magnitude as `subagent.ts`'s `MAX_SUBAGENT_ITERATIONS` (15), a
 * little higher since a side task can be a somewhat broader piece of work
 * than a narrowly-scoped subagent delegation but is still meant to be a
 * bounded side pane, not an open-ended agent. */
export const MAX_SIDE_TASK_ITERATIONS = 20;

/** Caps the final report string shown in the side task's card and offered to
 * "Promote" — mirrors `subagent.ts`'s `MAX_REPORT_CHARS`, sized a bit larger
 * since a side task's report is read directly by the user in its own panel,
 * not folded back into a parent turn's own context budget. */
const MAX_REPORT_CHARS = 20_000;

/** Bound on a tool-evidence row's args/result preview — this is a UI-only
 * bounded summary (`SideTaskToolEvidence.argsPreview`/`resultPreview`), never
 * the actual value handed to the model (that still flows through
 * `executeToolCall`/the wire history unabridged, same as everywhere else in
 * the app). */
const MAX_EVIDENCE_PREVIEW_CHARS = 400;

function capReport(text: string): string {
  if (text.length <= MAX_REPORT_CHARS) return text;
  return `${text.slice(0, MAX_REPORT_CHARS)}\n\n[Report truncated — side task's final reply exceeded ${MAX_REPORT_CHARS} characters]`;
}

function previewText(value: string): string {
  const trimmed = value.trim();
  if (trimmed.length <= MAX_EVIDENCE_PREVIEW_CHARS) return trimmed;
  return `${trimmed.slice(0, MAX_EVIDENCE_PREVIEW_CHARS)}…`;
}

/** Short `name(args preview)` label for a tool-evidence row — same idea as
 * `subagent.ts`'s private `activityLabel`, kept as its own small copy here
 * rather than a shared import (this module must not depend on `subagent.ts`,
 * and vice versa: they're sibling engines, not layered on each other). */
function argsPreview(toolCall: ToolCall): string {
  const raw = toolCall.function.arguments;
  if (!raw) return '';
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed && typeof parsed === 'object') {
      return previewText(
        Object.entries(parsed as Record<string, unknown>)
          .map(([key, value]) => `${key}: ${typeof value === 'string' ? value : JSON.stringify(value)}`)
          .join(', '),
      );
    }
  } catch {
    // Fall through to the raw string below.
  }
  return previewText(raw);
}

/** Whether a `write_file`/`edit_file` tool result represents success rather
 * than the `{"error": ...}` shape `stringifyToolError` produces — a tiny
 * copy of `subagent.ts`'s private `isSuccessfulMutationResult`/
 * `toolCallPathArg`, same "no cross-import between sibling engines" posture
 * as `argsPreview` above. */
function isSuccessfulMutationResult(resultContent: string): boolean {
  try {
    const parsed: unknown = JSON.parse(resultContent);
    return !(parsed && typeof parsed === 'object' && 'error' in parsed);
  } catch {
    return true;
  }
}

function toolCallPathArg(toolCall: ToolCall): string | null {
  try {
    const parsed: unknown = JSON.parse(toolCall.function.arguments || '{}');
    const path = (parsed as { path?: unknown } | null)?.path;
    return typeof path === 'string' ? path : null;
  } catch {
    return null;
  }
}

function classifyOutcome(resultContent: string, cancelled: boolean): SideTaskToolOutcome {
  if (cancelled) return 'cancelled';
  try {
    const parsed = JSON.parse(resultContent) as { error?: unknown };
    if (parsed && typeof parsed === 'object' && parsed.error) {
      return String(parsed.error).toLowerCase().includes('permission') ? 'denied' : 'failed';
    }
  } catch {
    // A successful plain-text tool result isn't JSON at all — expected.
  }
  return 'succeeded';
}

/** No side task profile ever offers an MCP tool (see `toolsForProfile`), so
 * — same reasoning as `subagent.ts`'s `emptyMcpRegistry` — an always-empty
 * registry is enough to satisfy `executeToolCall`'s signature. */
function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

/** The system prompt seeded into a side task's own local transcript.
 * Deliberately its own (small) builder rather than reusing
 * `systemPrompt.ts`'s `buildSubagentSystemPrompt`: that one's copy is
 * explicitly framed as "spawned by a coordinating AI agent", which would be
 * a wrong (and confusing, if the model ever quotes it back) description of
 * a task the USER started directly. */
function buildSideTaskSystemPrompt(
  roots: PromptWorkspaceRoot[],
  osLabel: string,
  profile: SideTaskProfile,
  title: string,
  source: SideTaskSource,
): string {
  const primary = roots.find((r) => r.is_primary) ?? null;
  const secondaries = roots.filter((r) => !r.is_primary);

  const workspaceLines = primary
    ? [
        `The primary workspace folder is "${primary.path}". Tool paths are relative to it.`,
        ...(secondaries.length > 0
          ? [
              `Additional attached folders (address them by prefixing paths with their label): ${secondaries
                .map((r) => `"${r.label}" (${r.path})`)
                .join(', ')}.`,
            ]
          : []),
      ]
    : ['No workspace folder is open yet. Tools will fail until the user opens one — say so in your report instead of retrying.'];

  const toolLines =
    profile === 'code'
      ? [
          'You have read-only tools (read_file, list_dir, glob, grep) plus write_file, edit_file, and run_shell to make changes. Mutating tools may prompt the user for permission and can be denied — if denied, stop and report that instead of retrying.',
        ]
      : ['You have read-only tools only: read_file, list_dir, glob, grep. You cannot write or edit files, or run shell commands.'];

  return [
    "You are a side-task agent running inside Little Monkey, a desktop AI app, started directly by the user — not by another AI — to work on one scoped task in parallel with their main chat.",
    `The user's operating system is ${osLabel}.`,
    '',
    ...workspaceLines,
    '',
    `Your task: ${title}`,
    `The user started this task from: ${source.label}.`,
    ...toolLines,
    '',
    'Complete the task, then reply with a report of what you found or did. Your reply is shown in a side pane, not the main chat — the user reads it there, can send you follow-up messages in that pane, and can choose to promote a reply into the chat, so make each reply stand on its own. If you get blocked, report what you found and why you stopped, then stop.',
  ].join('\n');
}

/** Per-attempt cancellation, keyed by `SideTaskRecord.id` (not `turnId` —
 * this is a process-local JS handle, not something the Rust side needs to
 * key on). Cleared once the run reaches a terminal state or is superseded by
 * a retry's own fresh entry. */
const controllers = new Map<string, AbortController>();

/** Resolves once `taskId`'s status is no longer `'paused'`, or `signal`
 * aborts — the mechanism that makes pause/resume actually hold the loop
 * between rounds without polling. Checked at the top of every iteration and
 * before every individual tool call, mirroring the existing "Stop takes
 * effect at the next safe checkpoint, never mid-stream" posture the rest of
 * the app's cancellation already has (see `turnEngine.ts`'s
 * `executeToolCall` abort race) — pause is the same idea, just resumable
 * instead of terminal. */
export function waitUntilResumed(taskId: string, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const stillPaused = () => useSideTaskStore.getState().tasks[taskId]?.status === 'paused';
    if (signal.aborted || !stillPaused()) {
      resolve();
      return;
    }
    const unsubscribe = useSideTaskStore.subscribe(() => {
      if (signal.aborted || !stillPaused()) {
        unsubscribe();
        signal.removeEventListener('abort', onAbort);
        resolve();
      }
    });
    const onAbort = () => {
      unsubscribe();
      resolve();
    };
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

/** Cancels a side task's in-flight (or paused) attempt: aborts this
 * module's own `AbortController` for it, which — via the exact same abort
 * race every other `executeToolCall`/`attemptStream` caller in the app
 * already has — cancels any running shell command and denies any pending
 * permission prompt scoped to just this task's own `turnId` (see
 * `tools.rs`'s `tools_cancel_running` doc comment: `turn_id` of `Some`
 * scopes the cancellation, so the main chat's own in-flight tool call or
 * pending prompt is never touched). A no-op if the task isn't running. */
export function cancelSideTask(taskId: string): void {
  controllers.get(taskId)?.abort();
}

export function pauseSideTask(taskId: string): void {
  useSideTaskStore.getState().pause(taskId);
}

export function resumeSideTask(taskId: string): void {
  useSideTaskStore.getState().resume(taskId);
}

/** Builds the "produced artifacts" list (acceptance criterion: outputs
 * traceable to tool evidence) from this attempt's own local transcript and
 * mutated-path set — fenced html/svg/mermaid blocks in the final report via
 * `artifacts.ts`'s `extractArtifacts` (same detector the main chat's
 * `ArtifactPane` uses), plus every path a `code`-profile task successfully
 * wrote/edited. Pure function of already-collected state, called once the
 * run reaches a terminal status. */
function buildArtifacts(messages: ChatMessage[], mutatedPaths: readonly string[]): SideTaskArtifact[] {
  const fenceArtifacts: SideTaskArtifact[] = extractArtifacts(messages).map((block, index) => ({
    id: `fence-${index}-${block.ref.messageIndex}-${block.ref.blockIndex}`,
    kind: 'fence',
    label: `${block.title} (${block.kind})`,
    preview: previewText(block.content),
  }));
  const fileArtifacts: SideTaskArtifact[] = [...new Set(mutatedPaths)].map((path) => ({
    id: `file-${path}`,
    kind: 'file',
    label: path,
    preview: '',
  }));
  return [...fileArtifacts, ...fenceArtifacts];
}

export interface StartSideTaskParams {
  title: string;
  prompt: string;
  profile: SideTaskProfile;
  source: SideTaskSource;
  sessionId: string;
  /** Id of the side task being retried, if this is a retry — threaded
   * straight into `sideTaskStore.create`'s `retryOf` for lineage. */
  retryOf?: string | null;
}

/**
 * Creates a new `SideTaskRecord` (status `'queued'`) and fires off its loop
 * WITHOUT awaiting it — the defining "doesn't block the main chat" property
 * (ROADMAP.md acceptance: "A side task can run without blocking the main
 * chat"). Returns the new task's id immediately so a caller (a composer
 * form, a retry button) can select/reveal it right away.
 */
export function startSideTask(params: StartSideTaskParams): string {
  const record = useSideTaskStore.getState().create({
    title: params.title,
    prompt: params.prompt,
    profile: params.profile,
    source: params.source,
    sessionId: params.sessionId,
    // Placeholder until `resolveTarget()` resolves below — the card shows
    // "queued" status in the meantime rather than a misleading empty label.
    modelLabel: 'Resolving model…',
    retryOf: params.retryOf ?? null,
  });
  void runSideTask(record.id);
  return record.id;
}

/** Drives one side task attempt to a terminal state. Exported (in addition
 * to `startSideTask`, the fire-and-forget entry point every real caller
 * uses) purely so `sideTaskRunner.test.ts` can `await` a run directly rather
 * than polling the store for completion after a synchronous
 * `startSideTask` call — mirrors `subagent.ts`'s `runSubagentTask` being the
 * (there, only) directly-awaitable export its own test file uses the same
 * way. */
export async function runSideTask(taskId: string): Promise<void> {
  const controller = new AbortController();
  controllers.set(taskId, controller);
  const mutatedPaths: string[] = [];

  // Projected onto the unified process table. A side task is deliberately NOT a
  // child of the chat turn that started it — that is the whole point of a side
  // task, and `sessionId` on the record is an association, not a parent process.
  // Fail-soft — see `processTable.ts`.
  const processIdPromise = admitProcess({
    kind: 'side_task',
    externalId: taskId,
    profile: useSideTaskStore.getState().tasks[taskId]?.profile ?? null,
  }).then(async (id) => {
    if (id) await markProcessRunning(id);
    return id;
  });

  const finishTerminal = (
    status: 'completed' | 'error' | 'cancelled',
    finalReport: string | null,
    error: string | null,
  ): void => {
    const store = useSideTaskStore.getState();
    const task = store.tasks[taskId];
    if (task) store.setArtifacts(taskId, buildArtifacts(task.messages, mutatedPaths));
    store.finish(taskId, status, finalReport, error);
    controllers.delete(taskId);
    // Not awaited: this helper is synchronous and is the single terminal path
    // every outcome routes through, so blocking it on IPC would change the
    // loop's shape.
    void processIdPromise.then((id) => {
      if (!id) return;
      const exitStatus =
        status === 'completed' ? 'succeeded' : status === 'cancelled' ? 'cancelled' : 'failed';
      return exitProcess(id, exitStatus, error ?? null);
    });
  };

  try {
    let target: ResolvedTarget;
    try {
      target = await resolveTarget();
    } catch (err) {
      finishTerminal('error', null, errorMessage(err));
      return;
    }
    if (controller.signal.aborted) {
      finishTerminal('cancelled', null, null);
      return;
    }

    const modelLabel = describeUsageTarget(target);
    useSideTaskStore.setState((state) => {
      const existing = state.tasks[taskId];
      if (!existing) return state;
      return { tasks: { ...state.tasks, [taskId]: { ...existing, modelLabel } } };
    });
    useSideTaskStore.getState().markRunning(taskId);

    const initialTask = useSideTaskStore.getState().tasks[taskId];
    if (!initialTask) return;

    const roots: PromptWorkspaceRoot[] = useWorkspaceStore.getState().roots;
    const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
    const systemPrompt = buildSideTaskSystemPrompt(roots, osLabel, initialTask.profile, initialTask.title, initialTask.source);
    const tools: ToolDef[] = toolsForProfile(initialTask.profile);
    const mcpRegistry = emptyMcpRegistry();
    const agentLabel = `Side task "${initialTask.title}"`;

    let messages: ChatMessage[] = initialTask.messages;

    for (let iteration = 0; iteration < MAX_SIDE_TASK_ITERATIONS; iteration++) {
      await waitUntilResumed(taskId, controller.signal);
      if (controller.signal.aborted) return finishTerminal('cancelled', null, null);

      const wireHistory: ChatMessage[] = [{ role: 'system', content: systemPrompt }, ...messages];

      // `recordUsage: false` — a side task's token usage must never clobber
      // the parent chat SESSION's own context-usage ring (same reasoning as
      // `subagent.ts`'s identical choice); it's still recorded into the
      // global `useUsageHistoryStore` ledger just below for cost visibility.
      const attempt = await attemptStream(target, wireHistory, tools, controller.signal, undefined, initialTask.sessionId, undefined, false);

      if (attempt.usage) {
        useSideTaskStore.getState().addUsage(taskId, attempt.usage);
        useUsageHistoryStore.getState().recordUsage(modelLabel, attempt.usage);
      }

      if (attempt.streamError !== null) {
        return finishTerminal('error', null, attempt.streamError);
      }

      if (attempt.toolCalls.length === 0) {
        if (controller.signal.aborted && attempt.content.length === 0) return finishTerminal('cancelled', null, null);
        const finalMessage: ChatMessage = { role: 'assistant', content: attempt.content };
        messages = [...messages, finalMessage];
        useSideTaskStore.getState().appendMessage(taskId, finalMessage);
        return finishTerminal('completed', capReport(attempt.content.trim() || '(side task finished with no report)'), null);
      }

      const assistantMessage: ChatMessage = { role: 'assistant', content: attempt.content, tool_calls: attempt.toolCalls };
      messages = [...messages, assistantMessage];
      useSideTaskStore.getState().appendMessage(taskId, assistantMessage);

      for (const toolCall of attempt.toolCalls) {
        await waitUntilResumed(taskId, controller.signal);
        const aborted = controller.signal.aborted;

        useSideTaskStore.getState().recordToolProposed(taskId, {
          id: toolCall.id,
          name: toolCall.function.name,
          argsPreview: argsPreview(toolCall),
          resultPreview: '',
          outcome: 'pending',
          startedAt: Date.now(),
          finishedAt: null,
        });

        const allowed = isToolCallAllowed(toolCall, tools);
        const resultContent = aborted
          ? CANCELLED_TOOL_RESULT
          : !allowed
            ? stringifyToolError(new Error(`Tool "${toolCall.function.name}" was not offered to this side task and was not executed.`))
            : // `checkpointId: null` — a side task's mutations go through the
              // same write_file/edit_file permission gate as everything
              // else, but are not linked into any chat session's checkpoint
              // undo timeline (a side task isn't "inside" any single chat
              // turn). `initialTask.turnId` scopes the permission
              // request/grant/cancellation to just this task (see this
              // module's doc comment). `agentLabel` attributes the prompt
              // in `PermissionModal.tsx`, same field subagents already use.
              await executeToolCall(toolCall, null, initialTask.turnId, mcpRegistry, controller.signal, undefined, undefined, undefined, agentLabel);

        const outcome = classifyOutcome(resultContent, aborted);
        useSideTaskStore.getState().recordToolFinished(taskId, toolCall.id, outcome, previewText(resultContent));

        const toolMessage: ChatMessage = {
          role: 'tool',
          tool_call_id: toolCall.id,
          content: allowed ? protectToolResult(toolCall.function.name, resultContent, false) : resultContent,
        };
        messages = [...messages, toolMessage];
        useSideTaskStore.getState().appendMessage(taskId, toolMessage);

        if (
          !aborted &&
          allowed &&
          (toolCall.function.name === 'write_file' || toolCall.function.name === 'edit_file') &&
          isSuccessfulMutationResult(resultContent)
        ) {
          const path = toolCallPathArg(toolCall);
          if (path) mutatedPaths.push(path);
        }
      }

      if (controller.signal.aborted) return finishTerminal('cancelled', null, null);
    }

    return finishTerminal(
      'error',
      null,
      `Side task stopped after reaching the safety limit of ${MAX_SIDE_TASK_ITERATIONS} tool-calling iterations without a final answer.`,
    );
  } catch (err) {
    finishTerminal('error', null, errorMessage(err));
  }
}

/**
 * Sends a follow-up message to an already-finished side task and runs the
 * next turn on the SAME record — what makes a side task a conversation the
 * user can keep talking to in its own pane (`SideTaskPane.tsx`), rather than
 * a one-shot report. The task's existing transcript is the context, so the
 * follow-up sees everything the first turn did.
 *
 * A follow-up mints a BRAND-NEW `turnId`, exactly like `retrySideTask` does
 * and for the same reason (see `SideTaskRecord.turnId`): an "allow for this
 * run" grant the user gave the previous turn must not silently authorize
 * whatever this new instruction asks for.
 *
 * Refuses (returns `false`) while the task is still queued/running/paused —
 * the composer disables itself in those states, and this is the backstop.
 */
export function continueSideTask(taskId: string, message: string): boolean {
  const store = useSideTaskStore.getState();
  const task = store.tasks[taskId];
  if (!task) return false;
  if (task.status === 'queued' || task.status === 'running' || task.status === 'paused') return false;
  const text = message.trim();
  if (!text) return false;

  store.appendMessage(taskId, { role: 'user', content: text });
  useSideTaskStore.setState((state) => {
    const existing = state.tasks[taskId];
    if (!existing) return state;
    return {
      tasks: {
        ...state.tasks,
        [taskId]: {
          ...existing,
          turnId: crypto.randomUUID(),
          status: 'queued',
          finalReport: null,
          error: null,
          finishedAt: null,
          updatedAt: Date.now(),
        },
      },
    };
  });
  void runSideTask(taskId);
  return true;
}

/** Starts a brand-new attempt from an existing (terminal) side task's own
 * frozen `prompt`/`profile`/`source` — a fresh `SideTaskRecord` with its own
 * id and `turnId` (see `SideTaskRecord.turnId`'s doc comment for why reusing
 * the old one would be unsafe), linked back via `retryOf`. Does nothing if
 * `taskId` doesn't exist. Returns the new task's id, or null. */
export function retrySideTask(taskId: string): string | null {
  const source = useSideTaskStore.getState().tasks[taskId];
  if (!source) return null;
  return startSideTask({
    title: source.title,
    prompt: source.prompt,
    profile: source.profile,
    source: source.source,
    sessionId: source.sessionId,
    retryOf: taskId,
  });
}

export { buildArtifacts as buildSideTaskArtifacts };

/**
 * "Promote side-task output back into the main chat only by user action"
 * (ROADMAP.md acceptance) — the ONLY function in this module that ever
 * calls `sessionStore`'s `addMessage`, and it is only ever invoked from a
 * user click on a "Promote" button (`SideTaskDetail.tsx`), never from
 * anywhere inside `runSideTask` itself. Appends the task's final report as
 * a plain assistant-role message to its originating chat session, prefixed
 * with a small provenance line (task title, tool-call count, model) so the
 * transcript itself records where the content came from — no auto-injection
 * path exists anywhere else in this module. Returns `false` (no-op) if the
 * task has no report yet (still running, or ended in error/cancellation
 * with nothing to show).
 */
export function promoteSideTask(taskId: string): boolean {
  const store = useSideTaskStore.getState();
  const task = store.tasks[taskId];
  if (!task || !task.finalReport) return false;
  const toolCount = task.toolEvidence.length;
  const header = `_Promoted from side task "${task.title}" (${toolCount} tool call${toolCount === 1 ? '' : 's'} · ${task.modelLabel})_`;
  useSessionStore.getState().addMessage(task.sessionId, {
    role: 'assistant',
    content: `${header}\n\n${task.finalReport}`,
  });
  store.markPromoted(taskId);
  return true;
}

/**
 * "Open as full task" (ROADMAP.md acceptance) — graduates a side task into
 * a brand-new, first-class chat session: a fresh session seeded with the
 * task's own frozen prompt as the first user message (so it carries the
 * full main-chat toolset, checkpoints, and MCP access a side task
 * deliberately doesn't have), plus the side task's own final report as the
 * next assistant message when one exists, so context isn't lost switching
 * surfaces. Returns the new session's id (also the app's newly active
 * session — same "graduated to a new tab" convention `newSession()` already
 * has), or null if `taskId` doesn't exist.
 */
export function openSideTaskAsFullChat(taskId: string): string | null {
  const task = useSideTaskStore.getState().tasks[taskId];
  if (!task) return null;
  const sessionStore = useSessionStore.getState();
  sessionStore.newSession();
  const newSessionId = useSessionStore.getState().activeSessionId;
  useSessionStore.getState().addMessage(newSessionId, { role: 'user', content: task.prompt });
  if (task.finalReport) {
    useSessionStore.getState().addMessage(newSessionId, { role: 'assistant', content: task.finalReport });
  }
  useSessionStore.getState().renameSession(newSessionId, `Side task: ${task.title}`);
  return newSessionId;
}
