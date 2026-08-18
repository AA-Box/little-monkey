/**
 * The agentic tool-calling loop.
 *
 * Mirrors how Claude Code itself works: send the conversation (plus the
 * available tool definitions) to the model, stream its reply into the chat,
 * and whenever the model asks for tool calls, execute them through Tauri's
 * sandboxed + permission-gated commands (see src-tauri/src/tools.rs), feed
 * the results back as `tool` messages, and repeat. The loop ends as soon as
 * a turn produces a plain answer with no tool calls, or after MAX_ITERATIONS
 * round trips as a safety cap against runaway/looping models.
 *
 * On top of that base loop, this module also owns three client-side
 * reliability behaviors (see the plan this was built from — no server-side
 * gateway involved, this app is single-user/local):
 *  - Auto-failover across configured cloud providers when one errors before
 *    any content streams back (never mid-stream — see `attemptStream`).
 *  - Vision-aware auto-switch: if an image is attached and the active model
 *    can't see, switch to one that can before the turn starts.
 *  - Adaptive context compaction once history crosses a configured
 *    percentage of the active model's context window (see
 *    `contextTrimmer.ts`).
 * Both switches reuse the existing `modelStore` active-target setters, so
 * "session affinity" (keep using whatever just worked) falls out of the
 * same mechanism a manual model switch uses — no separate sticky field.
 */
import { invoke, isTauri } from '@tauri-apps/api/core';
import { allowedToolsRestriction, applyAllowedToolsRestriction } from './allowedTools';
import { textContent } from './llamaClient';
import type { ChatContentPart, ChatMessage, ToolCall, ToolDef } from './llamaClient';
import { GENERATE_IMAGE_TOOL, MANAGE_SKILL_LEARNING_TOOL, PRESENT_PLAN_TOOL, READ_SKILL_RESOURCE_TOOL, SKILL_INVOKE_TOOL, TASK_TOOL, WORKFLOW_TOOL, buildTools, toolsForWorkspace } from './tools';
import {
  candidateNotice,
  finalizeLearningForRun,
  formatLearningNotice,
  learnFromFinishedRun,
  type InvokedSkillUse,
  type ReflectionCall,
} from './skillLearning';
import { cachedLearningMode } from './skillLearningClient';
import type { NativeSkillScope } from './nativeSkillsClient';
import { mcpToolDefs } from './mcpTools';
import { executableExtensionToolDefs } from './executableExtensionTools';
import { isVisionCapableLocalModel, isVisionCapableOllamaModel, isVisionCapableProviderModel } from './visionModels';
import { applyContextCompaction, renderForSummary, shouldTrim } from './contextTrimmer';
import {
  abortedPromise,
  attemptStream,
  CANCELLED_TOOL_RESULT,
  describeUsageTarget,
  executeToolCall,
  isBlockedInPlanMode,
  isToolCallAllowed,
  PRESENT_PLAN_RESULT,
  stringifyToolError,
  type ResolvedTarget,
  type RiskAnnotationContext,
  type SkillToolContext,
  type SubagentContext,
} from './turnEngine';
import { classifyToolCall, type RiskClassification } from './riskJudge';
import {
  composeReferencedText,
  extractMentionPaths,
  formatDirListing,
  type DirEntry,
  type ResolvedTextReference,
} from './mentions';
import { currentSystemPrompt, ULTRACODE_SYSTEM_SECTION, type AttachedStackPromptInfo } from './systemPrompt';
import { composeSkillCatalog, composeSkillSystemPrompt, MAX_SKILLS_PER_TURN, type SkillInvocationSnapshot, type SlashSkill } from './skills';
import { composeSavedWorkflowCatalog } from './workflow';
import { selectSavedWorkflowList, useSavedWorkflowStore } from '../store/savedWorkflowStore';
import { composeCustomAgentCatalog } from './customAgents';
import { selectCustomAgentList, useCustomAgentStore } from '../store/customAgentStore';
import { collectUserPromptSubmitContext } from './userHooks';
import { protectKnowledgeNoticeForModel, protectToolResult } from './untrustedContent';
import { isBtwNotice } from './slashCommands';
import { sessionMessages, useSessionStore } from '../store/sessionStore';
import { effortForTarget, useModelStore } from '../store/modelStore';
import { useUsageStore } from '../store/usageStore';
import { useUsageHistoryStore } from '../store/usageHistoryStore';
import { useTurnStatusStore } from '../store/turnStatusStore';
import { useSettingsStore } from '../store/settingsStore';
import { useRulesStore } from '../store/rulesStore';
import { useCheckpointStore } from '../store/checkpointStore';
import { usePermissionStore, type PermissionMode } from '../store/permissionStore';
import { useStackStore, type StackQueryResult } from '../store/stackStore';
import { useMcpStore } from '../store/mcpStore';
import { primaryRoot, useWorkspaceStore } from '../store/workspaceStore';
import { admitProcess, exitProcess, exitStatusFor, linkProcessRun, markProcessRunning, reconcileProcess } from './processTable';
import { honourPause, forgetPause, isPauseRequested } from './pauseRegistry';
import { usePrivacyFirewallStore } from '../store/privacyFirewallStore';
import {
  describeRedactions,
  gatePrivacyWireMessages,
  type PrivacyWireCache,
} from './privacyWire';
import { evaluateRateLimit, recordRequest } from './rateLimitTracker';
import {
  assertCostBudgetAllowsRequest,
  useCostControlStore,
} from '../store/costControlStore';
import type { VerifyConfig } from '../store/verifyStore';
import { extractArtifacts } from './artifacts';
import { useArtifactStore } from '../store/artifactStore';
// K9 target resolution and dispatch policy, lifted into their own module so
// `subagent.ts` can route without importing this one — see `targetRouting.ts`.
// Re-exported below because ~70 modules already read them through here.
import {
  applyTargetSwitch,
  resolveTarget,
  resolvedTargetSupportsVision,
  routeFromActive,
  routeTarget,
  snapshotForResolvedTarget,
  targetLabel,
  type RoutedTarget,
  type RoutingContext,
} from './targetRouting';

export {
  resolveTarget,
  routeFromActive,
  routeTarget,
  snapshotForResolvedTarget,
  type RoutedTarget,
  type RoutingContext,
};
import { beginDurableRun, type DurableRunRecorder } from './durableRun';
import { daemonCancel } from './daemonClient';
import { requestRunCancellation } from './runProtocol';
import { registerRunCancellation } from './runCancellationRegistry';
import {
  buildDaemonDesktopRecipe,
  daemonDesktopRoute,
  loadActiveDaemonTurns,
  removeActiveDaemonTurn,
  saveActiveDaemonTurn,
  submitDaemonDesktopTurn,
  watchDaemonDesktopTurn,
  type AcceptedResume,
  type ActiveDaemonDesktopTurn,
  type ConversationRoute,
  type DesktopTurnSource,
  type FrozenAttachmentInput,
} from './daemonDesktopTurn';
import {
  canRetryWithoutTools,
  mutationAttemptFailureMessage,
  mutationPlainResponseAction,
  mutationToolFailureReason,
  requiresWorkspaceMutation,
  workspaceMutationPreflightFailure,
  WORKSPACE_MUTATION_CORRECTION,
  WORKSPACE_MUTATION_FAILURE,
} from './workspaceMutation';
import { errorMessage } from "./errors";

/** Hard cap on model/tool round trips for a single call to runAgentTurn. */
const MAX_ITERATIONS = 25;

/** Prefix identifying a synthetic model-switch notice (auto-failover or
 * vision auto-switch) inserted into the transcript — mirrors
 * `contextTrimmer.ts`'s `COMPACTION_MARKER_PREFIX` pattern so `MessageList`
 * can recognize and render both kinds of system-role notice distinctly from
 * a real (currently nonexistent, but defensively still hidden) system
 * message. */
export const SWITCH_NOTE_PREFIX = '[Model switch]';

/** Marks the notice a resumed turn writes — see {@link ResumedTurn}. Lives here
 * rather than in `frozenTurn.ts` so the edge between the two modules runs one
 * way: `frozenTurn` needs `runAgentTurn`, and a constant read during module
 * initialization on the way back would be the half of a cycle that bites. */
export const RESUME_NOTE_PREFIX = '[Resume]';

/** A re-entry into a turn frozen at a tool boundary (roadmap K13), passed to
 * {@link runAgentTurn} by `frozenTurn.ts` and by nothing else.
 *
 * Every field describes something that has *already happened*. Nothing in this
 * type is a request to submit a Resume — by the time one of these exists, the
 * backend holds the continuation durably, and the only thing left is to watch
 * it. That ordering is the whole point: submission has to be the step the frozen
 * image outlives, so it cannot be the step this loop performs. */
export interface ResumedTurn {
  /** The image that was continued. Cleared by the caller once the continuation
   * below was durably accepted — held here for the transcript notice and for
   * anything that later wants to name it. */
  resumedFromCheckpointId: string;
  /** `checkpoint_restorability`'s statement of what a resume does *not*
   * reproduce, written into the transcript beside the continuation. */
  determinismCaveats: string[];
  /** The turn the frozen image belongs to — the `chat_turn` process's own
   * external id.
   *
   * This is what makes a resume a *continuation* rather than a new turn: it is
   * half of the accepted turn's durable identity, and it is what the backend
   * found the row by. A watcher reconnects by asking about *this* turn, because
   * the continuation's own id is the backend's business. */
  parentTurnId: string;
  /** The continuation the backend accepted, as it answered. Its run is what
   * this loop attaches the transcript to. */
  accepted: AcceptedResume;
}

export function isSwitchNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(SWITCH_NOTE_PREFIX);
}

/** Matches the OpenRouter (and OpenAI-compat-alike) 404 body some free/small
 * models return when tools are merely *offered* in the request, even for a
 * turn the model never intended to call one — every turn offers the full
 * tool list (see `toolsForTurn` below), so without this the model is
 * unusable for plain chat too. Caught in `runAgentTurn` to retry the same
 * target once with an empty tool list rather than surfacing a raw error or
 * failing the target over. */
const TOOL_UNSUPPORTED_ERROR_PATTERN = /support(?:s|ing)? tool use/i;

/** Prefix identifying a synthetic notice listing "@"-mentions that failed to
 * resolve this turn (typo'd path, unreadable file — see `resolveReferences`)
 * — same pattern as `SWITCH_NOTE_PREFIX`, so the user learns why the model
 * never saw the file instead of the failure being swallowed silently. */
export const MENTION_NOTE_PREFIX = '[Mentions]';

export function isMentionNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(MENTION_NOTE_PREFIX);
}

/** Prefix identifying a synthetic Privacy Firewall notice (ROADMAP.md Phase
 * 5) — redaction, block, or local-only-fallback switch — inserted into the
 * transcript by the pre-turn gate in `runAgentTurnBody`. Same rendering
 * convention as `SWITCH_NOTE_PREFIX`/`MENTION_NOTE_PREFIX` above. */
export const PRIVACY_NOTE_PREFIX = '[Privacy firewall]';

export function isPrivacyNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(PRIVACY_NOTE_PREFIX);
}

/**
 * Builds the `{is, parse, format}` trio every `[Prefix]{json}` synthetic
 * notice below (Checkpoint/Plan/Memory/Verify/Sources) needs — before this
 * factory each of those five was an independent hand-rolled copy of the same
 * three functions (a `startsWith` check, a `JSON.parse` wrapped in a
 * try/catch that degrades to `null` on anything malformed, and a
 * `JSON.stringify` back onto the prefix). `isValid` — the payload's own
 * shape check — is the only thing that actually varies per notice type; it's
 * supplied as a type guard so `parse`'s return type narrows to `T` without a
 * cast at every call site. Plain-text notices with no JSON payload
 * (`SWITCH_NOTE_PREFIX`/`MENTION_NOTE_PREFIX` above, `VERIFY_FIX_NOTE_PREFIX`
 * below) deliberately stay outside this factory — there's no payload to
 * parse, so wrapping them here would just be a `parse`/`format` pair nobody
 * calls.
 */
function makeNotice<T>(prefix: string, isValid: (value: unknown) => value is T) {
  function is(message: ChatMessage): boolean {
    return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(prefix);
  }
  function parse(message: ChatMessage): T | null {
    if (!is(message)) return null;
    try {
      const parsed: unknown = JSON.parse((message.content as string).slice(prefix.length));
      if (isValid(parsed)) return parsed;
    } catch {
      // Malformed payload — treat as "not this notice type".
    }
    return null;
  }
  function format(notice: T): string {
    return `${prefix}${JSON.stringify(notice)}`;
  }
  return { is, parse, format };
}

/** Prefix identifying a synthetic per-turn checkpoint notice inserted into
 * the transcript after a turn that mutated files — same pattern as
 * `SWITCH_NOTE_PREFIX`. The rest of the content is a JSON payload (see
 * `CheckpointNotice`), so `MessageList` can render a Revert button for it. */
export const CHECKPOINT_NOTE_PREFIX = '[Checkpoint]';

/** How much of the user prompt is kept as the checkpoint's label — used for
 * timeline display and for validating the rewind anchor (see
 * `checkpointAnchorValid`). Mirrors the manifest's `label` field cap. */
export const CHECKPOINT_LABEL_MAX_CHARS = 120;

/** Payload embedded in a checkpoint notice message. */
export interface CheckpointNotice {
  id: string;
  /** Absolute paths of every file the turn mutated. */
  files: string[];
  /** Index of the turn's user message in the transcript — the target for
   * "Rewind conversation". Absent on notices recorded before manifest v2
   * (those degrade to file-only restore). */
  anchorIndex?: number;
  /** First ~120 chars of the prompt that started the turn — validates that
   * `anchorIndex` still points at the same message after compaction or
   * edit-and-resubmit shifted indices. */
  label?: string;
  /** True if `run_shell` executed during the turn (see `record_shell` in
   * checkpoints.rs) — file restore may not undo everything, since shell side
   * effects are never snapshotted. */
  shellRan?: boolean;
  /** Set once the user has reverted this checkpoint. */
  reverted?: boolean;
}

/**
 * Whether `notice`'s conversation-rewind anchor still points at the user
 * message that started its turn. Context compaction and edit-and-resubmit
 * both shift transcript indices, so before offering "Rewind conversation"
 * the anchored message must still be a user message whose text starts with
 * the notice's label — otherwise the UI degrades to file-only restore.
 */
export function checkpointAnchorValid(messages: ChatMessage[], notice: CheckpointNotice): boolean {
  if (typeof notice.anchorIndex !== 'number' || !Number.isInteger(notice.anchorIndex) || notice.anchorIndex < 0) {
    return false;
  }
  if (typeof notice.label !== 'string') return false;
  const anchored = messages[notice.anchorIndex];
  if (!anchored || anchored.role !== 'user') return false;
  return textContent(anchored.content).startsWith(notice.label);
}

/** Minimal shape `checkpointChainBlockReason` needs from a `CheckpointInfo`
 * (see `src/store/checkpointStore.ts`) — kept local rather than imported to
 * avoid a store <-> lib import cycle. */
export interface CheckpointChainLink {
  id: string;
  shellRan: boolean;
  prevId?: string | null;
}

/** Why (if at all) `CheckpointTimeline.tsx`'s "Restore to here" should be
 * disabled for the checkpoint at `targetIndex`, or `null` if the newest→
 * target chain is safe to revert in full. */
export type CheckpointChainBlockReason = 'shellRan' | 'prunedGap' | null;

/**
 * Scans `checkpoints` (newest-first, as returned by `checkpoint_list`) from
 * index 0 through `targetIndex` inclusive for anything that makes "Restore
 * to here" unsafe across that span:
 * - `'prunedGap'`: a checkpoint's recorded `prevId` doesn't match the id of
 *   the next-older entry in the chain, meaning something in between was
 *   pruned off disk and its changes can no longer be reverted.
 * - `'shellRan'`: a shell command ran during one of these turns, so file
 *   restore alone can't guarantee full coverage.
 * A pruned gap is checked first since it's the harder guarantee failure —
 * shell coverage is merely partial, a pruned checkpoint's changes are gone
 * entirely.
 */
export function checkpointChainBlockReason(checkpoints: CheckpointChainLink[], targetIndex: number): CheckpointChainBlockReason {
  const hasPrunedGap = checkpoints.slice(0, targetIndex).some((c, i) => {
    const next = checkpoints[i + 1];
    return Boolean(c.prevId) && Boolean(next) && next.id !== c.prevId;
  });
  if (hasPrunedGap) return 'prunedGap';

  const hasShellRan = checkpoints.slice(0, targetIndex + 1).some((c) => c.shellRan);
  if (hasShellRan) return 'shellRan';

  return null;
}

function isCheckpointPayload(value: unknown): value is CheckpointNotice {
  return (
    !!value &&
    typeof value === 'object' &&
    typeof (value as CheckpointNotice).id === 'string' &&
    Array.isArray((value as CheckpointNotice).files)
  );
}

const checkpointNoticeCodec = makeNotice<CheckpointNotice>(CHECKPOINT_NOTE_PREFIX, isCheckpointPayload);

export const isCheckpointNotice = checkpointNoticeCodec.is;
/** Parses a checkpoint notice's JSON payload; `null` for anything malformed. */
export const parseCheckpointNotice = checkpointNoticeCodec.parse;
/** Serializes a checkpoint notice back into message content — used both when
 * the notice is first added and when the Revert button marks it reverted. */
export const formatCheckpointNotice = checkpointNoticeCodec.format;

/** Prefix identifying a synthetic notice inserted after a `present_plan` tool
 * call (Plan Mode only — see `toolsForMode`) — cloned from the
 * `CHECKPOINT_NOTE_PREFIX` pattern above. The rest of the content is a JSON
 * payload (see `PlanNotice`), so `MessageList` can render a `PlanCard` with
 * "Approve & start acting" / "Keep planning" buttons for it. */
export const PLAN_NOTE_PREFIX = '[Plan]';

/** Payload embedded in a plan notice message. */
export interface PlanNotice {
  id: string;
  title: string;
  /** The plan body, as Markdown — rendered by `PlanCard` with the same
   * `prose` classes `MessageBubble` uses for assistant messages. */
  plan: string;
  openQuestions?: string[];
  /** `'proposed'` until the user acts on the card; `'approved'` after
   * "Approve & start acting", `'dismissed'` after "Keep planning". Both
   * terminal states rewrite the notice in place (`updateMessageAt`), same
   * pattern as `CheckpointNotice.reverted`/`MemoryNotice.forgotten`. */
  status: 'proposed' | 'approved' | 'dismissed';
}

function isPlanPayload(value: unknown): value is PlanNotice {
  const openQuestions = (value as PlanNotice | null)?.openQuestions;
  return Boolean(
    value &&
      typeof value === 'object' &&
      typeof (value as PlanNotice).id === 'string' &&
      typeof (value as PlanNotice).title === 'string' &&
      typeof (value as PlanNotice).plan === 'string' &&
      typeof (value as PlanNotice).status === 'string' &&
      // openQuestions is optional, but if present it must actually be a
      // string[], or PlanCard's `.map()` over it throws at render time (e.g.
      // a persisted/hand-edited session whose payload sets it to a truthy
      // non-array like a string).
      (openQuestions === undefined || (Array.isArray(openQuestions) && openQuestions.every((q) => typeof q === 'string'))),
  );
}

const planNoticeCodec = makeNotice<PlanNotice>(PLAN_NOTE_PREFIX, isPlanPayload);

export const isPlanNotice = planNoticeCodec.is;
/** Parses a plan notice's JSON payload; `null` for anything malformed. */
export const parsePlanNotice = planNoticeCodec.parse;
/** Serializes a plan notice back into message content — used both when the
 * notice is first added and when Approve/Keep planning rewrite its status. */
export const formatPlanNotice = planNoticeCodec.format;

/**
 * Extracts a `present_plan` tool call's arguments into the fields a
 * `PlanNotice` needs (everything except `id`/`status`, which the caller
 * fills in). Never throws: malformed arguments JSON, or a missing
 * `title`/`plan` string, both degrade to `null` rather than throwing — same
 * "no path known" degradation `toolCallPathArg` uses for write/edit calls.
 * `open_questions` (the model's snake_case argument, matching the tool's
 * declared schema in `tools.ts`) is filtered down to just its string
 * entries and dropped entirely if empty or absent.
 */
export function toolCallPlanArgs(toolCall: ToolCall): { title: string; plan: string; openQuestions?: string[] } | null {
  try {
    const parsed: unknown = JSON.parse(toolCall.function.arguments || '{}');
    const title = (parsed as { title?: unknown } | null)?.title;
    const plan = (parsed as { plan?: unknown } | null)?.plan;
    if (typeof title !== 'string' || typeof plan !== 'string') return null;
    const rawQuestions = (parsed as { open_questions?: unknown } | null)?.open_questions;
    const openQuestions = Array.isArray(rawQuestions)
      ? rawQuestions.filter((q): q is string => typeof q === 'string')
      : [];
    return { title, plan, openQuestions: openQuestions.length > 0 ? openQuestions : undefined };
  } catch {
    return null;
  }
}

/**
 * Builds the tool list offered to the model this turn, appending
 * `PRESENT_PLAN_TOOL` only while `mode === 'plan'` — every other mode gets
 * `tools` back unchanged. Kept as its own pure function (mirroring
 * `toolsForSettings` just below) so it can be unit-tested and so the model's
 * offered tool list stays a single, easy-to-audit place: `present_plan` must
 * never be offered outside Plan Mode, since it has no purpose (and the
 * backend hard-blocks every other mutating tool in Plan Mode instead) once
 * the user has switched out of it. Named `toolsForMode` rather than the
 * design doc's `toolsForTurn` to avoid shadowing `runAgentTurnBody`'s own
 * `toolsForTurn` local (the final, fully-assembled per-turn tool list this
 * function's output feeds into, alongside `toolsForSettings`).
 */
export function toolsForMode(tools: ToolDef[], mode: PermissionMode): ToolDef[] {
  if (mode !== 'plan') return tools;
  // Fail closed at the OFFER level too, not just at Rust's mode gate: a tool
  // Plan Mode would refuse anyway (see `isBlockedInPlanMode` — mutating and
  // permission-gated names, `shell_kill`, and every un-marked `mcp__` tool)
  // is not even shown to the model, so a well-behaved model never wastes a
  // round trip on a doomed call. `executeToolCall`'s own check remains the
  // dispatch backstop for a model that emits one regardless.
  return [...tools.filter((tool) => !isBlockedInPlanMode(tool.function.name)), PRESENT_PLAN_TOOL];
}

/** Prefix identifying a synthetic notice inserted right after a successful
 * `remember` tool call — cloned from the `CHECKPOINT_NOTE_PREFIX` pattern
 * above. The rest of the content is a JSON payload (see `MemoryNotice`), so
 * `MessageList` can render a Forget button for it. */
export const MEMORY_NOTE_PREFIX = '[Memory]';

/** Payload embedded in a memory notice message. */
export interface MemoryNotice {
  id: string;
  text: string;
  /** Set once the user has forgotten this fact via the notice's Forget button. */
  forgotten?: boolean;
}

function isMemoryPayload(value: unknown): value is MemoryNotice {
  return (
    !!value &&
    typeof value === 'object' &&
    typeof (value as MemoryNotice).id === 'string' &&
    typeof (value as MemoryNotice).text === 'string'
  );
}

const memoryNoticeCodec = makeNotice<MemoryNotice>(MEMORY_NOTE_PREFIX, isMemoryPayload);

export const isMemoryNotice = memoryNoticeCodec.is;
/** Parses a memory notice's JSON payload; `null` for anything malformed. */
export const parseMemoryNotice = memoryNoticeCodec.parse;
/** Serializes a memory notice back into message content — used both when the
 * notice is first added and when the Forget button marks it forgotten. */
export const formatMemoryNotice = memoryNoticeCodec.format;

/** Prefix identifying a synthetic notice inserted after a turn that mutated
 * files ran the workspace's configured verification commands (see
 * `src-tauri/src/verify.rs`) — cloned from the `CHECKPOINT_NOTE_PREFIX`
 * pattern above. One notice is appended per command that ran, each carrying
 * a `VerifyNotice` JSON payload, so `MessageList` can render a labeled,
 * pass/fail, collapsible-output row per command. This slice (report-only,
 * `verifyMaxRounds` not yet implemented) never feeds a failure back to the
 * model — a later slice adds that on top of the same notice shape. */
export const VERIFY_NOTE_PREFIX = '[Verify]';

/** Payload embedded in a verify notice message. `code`/`output` mirror the
 * Rust `VerifyResult`'s `code`/combined `stdout`+`stderr` (see
 * `buildVerifyOutput`), `output` tail-capped again to
 * `VERIFY_NOTICE_OUTPUT_CAP` chars for the wire — `verify.rs` already caps
 * each stream at ~20k chars server-side, but stdout+stderr combined can still
 * exceed what's worth sending over IPC and storing in the transcript. */
export interface VerifyNotice {
  label: string;
  kind: string;
  /** `true` only when the command exited 0 and didn't time out. */
  ok: boolean;
  code: number | null;
  output: string;
  durationMs: number;
}

function isVerifyPayload(value: unknown): value is VerifyNotice {
  return (
    !!value &&
    typeof value === 'object' &&
    typeof (value as VerifyNotice).label === 'string' &&
    typeof (value as VerifyNotice).ok === 'boolean'
  );
}

const verifyNoticeCodec = makeNotice<VerifyNotice>(VERIFY_NOTE_PREFIX, isVerifyPayload);

export const isVerifyNotice = verifyNoticeCodec.is;
/** Parses a verify notice's JSON payload; `null` for anything malformed. */
export const parseVerifyNotice = verifyNoticeCodec.parse;
/** Serializes a verify notice back into message content. */
export const formatVerifyNotice = verifyNoticeCodec.format;

/** Prefix identifying the plain-text "fix this" instruction appended to the
 * transcript when a verification failure triggers a feed-back round (see
 * `runAgentTurnBody`'s use of `shouldFeedBackVerifyFailure`). Deliberately
 * NOT `VERIFY_NOTE_PREFIX`: that prefix's contract (see `isVerifyNotice`/
 * `parseVerifyNotice`) is "the rest of the content is a `VerifyNotice` JSON
 * payload", and this message is plain prose, not JSON — reusing the JSON
 * prefix here made `MessageList.tsx` match `isVerifyNotice`, fail to parse,
 * and silently drop the message from the timeline (the model still saw it
 * via `wireHistory`, but the user never did). Recognized by `MessageList.tsx`
 * as a plain-text notice, same rendering as `SWITCH_NOTE_PREFIX`/
 * `MENTION_NOTE_PREFIX` — this is an intentional deviation from the design
 * doc, which suggested reusing `VERIFY_NOTE_PREFIX` for this message. */
export const VERIFY_FIX_NOTE_PREFIX = '[Verify Fix]';

export function isVerifyFixNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(VERIFY_FIX_NOTE_PREFIX);
}

/** Tool names gated by the `webToolsEnabled` settings toggle — see `toolsForSettings`. */
const WEB_TOOL_NAMES = new Set(['web_fetch', 'web_search']);

/**
 * Filters `remember` out of the tool list offered to the model this turn
 * when the settingsStore `memoryEnabled` toggle is off, and/or `web_fetch`
 * and `web_search` out when `webToolsEnabled` is off, then APPENDS
 * `TASK_TOOL` when `subagentsEnabled` is on. This is the ONLY effect of any
 * of the three toggles — rules and previously-saved facts are still
 * injected into the system prompt unconditionally (see `runAgentTurnBody`'s
 * `useRulesStore.getState().refresh()` call); turning `memoryEnabled` off
 * stops the agent from saving *new* facts on its own, it is not amnesia.
 * Likewise, `webToolsEnabled` off just makes the two web tools invisible to
 * the model — it doesn't touch anything else.
 *
 * `subagentsEnabled` is deliberately handled in THIS function — the single
 * existing per-turn tool-list composer both Plan/Act (`toolsForMode`'s
 * `present_plan` append) and RAG (`buildTools`'s conditional `search_docs`
 * append) already extended — rather than a new parallel "toolsForSubagents"
 * filter: one composition chain
 * (`toolsForSettings(toolsForMode([...buildTools(...), ...mcpDefs], mode), ...)`,
 * see `runAgentTurnBody`) stays the one place to audit everything the model
 * is offered. Default `false` mirrors every other call site that doesn't
 * pass it, so `task` is never accidentally offered by an existing caller
 * that hasn't been updated for it.
 *
 * `skillToolEnabled`/`readSkillResourceToolEnabled` follow the same posture,
 * appending `SKILL_INVOKE_TOOL`/`READ_SKILL_RESOURCE_TOOL` — see
 * `runAgentTurnBody`'s call site for exactly what each is computed from.
 * They're independent: `readSkillResourceToolEnabled` is NOT gated on
 * `settingsStore.skillAutoInvokeEnabled` at all — reading a bundled file
 * should work for an explicitly `/command`-invoked skill too, not just an
 * auto-invoked one.
 */
export function toolsForSettings(
  tools: ToolDef[],
  memoryEnabled: boolean,
  webToolsEnabled = true,
  subagentsEnabled = false,
  skillToolEnabled = false,
  readSkillResourceToolEnabled = false,
  skillLearningToolEnabled = false,
): ToolDef[] {
  const filtered = tools.filter((tool) => {
    if (!memoryEnabled && tool.function.name === 'remember') return false;
    if (!webToolsEnabled && WEB_TOOL_NAMES.has(tool.function.name)) return false;
    return true;
  });
  return [
    ...filtered,
    ...(subagentsEnabled ? [TASK_TOOL, WORKFLOW_TOOL] : []),
    ...(skillToolEnabled ? [SKILL_INVOKE_TOOL] : []),
    ...(readSkillResourceToolEnabled ? [READ_SKILL_RESOURCE_TOOL] : []),
    ...(skillLearningToolEnabled ? [MANAGE_SKILL_LEARNING_TOOL] : []),
  ];
}

/**
 * `allowed-tools` enforcement (see each `SlashSkill.allowedTools`'s doc
 * comment): every skill invoked so far this turn (explicit AND
 * model-invoked, both live in `invokedCommands` — see `skillToolContext` in
 * `runAgentTurnBody`) that declares `allowedTools` narrows what's on offer.
 * Favors NOT restricting on ambiguity: if any invoked skill omits
 * `allowedTools` (or none are invoked yet), returns `null` (unrestricted);
 * only when EVERY invoked skill declares one does this return the union of
 * their sets, so stacking a restrictive skill with a permissive one never
 * silently locks the model out of tools the permissive skill actually needs.
 *
 * Implemented in `allowedTools.ts` and re-exported here, where every caller
 * already looks for it: `headlessAgentRunner.ts` applies the same narrowing to
 * an evaluation arm and cannot import this module.
 */
export { allowedToolsRestriction, applyAllowedToolsRestriction };

/** Minimal shape `attachedStackPromptInfo` needs from a `stackStore.ts`
 * `KnowledgeStack` — kept local (rather than importing the full interface)
 * since only these three fields matter for the derived description. */
interface AttachedStackLike {
  name: string;
  indexed_at: number | null;
  chunk_count: number;
}

/**
 * Derives each attached stack's short prompt-facing `description` (see
 * `systemPrompt.ts`'s `AttachedStackPromptInfo`) from its index status —
 * `chunk_count` once indexed, or a "not indexed yet" note so the model
 * doesn't expect `search_docs` to return anything for a stack that hasn't
 * been indexed at all. Pure so it can be unit-tested independent of
 * `stackStore`/`sessionStore`, same reasoning as `toolsForMode`/
 * `toolsForSettings` above.
 */
export function attachedStackPromptInfo(stacks: AttachedStackLike[]): AttachedStackPromptInfo[] {
  return stacks.map((stack) => ({
    name: stack.name,
    description:
      stack.indexed_at !== null ? `${stack.chunk_count} chunk${stack.chunk_count === 1 ? '' : 's'} indexed` : 'not indexed yet',
  }));
}

/** Prefix identifying a synthetic notice inserted before the first model call
 * of a turn when the session's doc-chat mode is on (see
 * `ChatSession.docChatMode`, `StackPicker.tsx`) — cloned from the
 * `CHECKPOINT_NOTE_PREFIX` pattern above. Carries the top-k passages
 * `stacks_query` retrieved for the user's own message, so `MessageList` can
 * render collapsible source chips and the model can answer with citations
 * instead of needing to call `search_docs` itself first. Added to the
 * transcript via the same `addMessage` every other notice in this module
 * uses — never handled as a separate "wire-only" message — which is also
 * what makes its token cost visible to `contextTrimmer.ts`'s
 * `estimateHistoryTokens` for free: that function sums every message in
 * history generically with no per-notice-type special-casing, so retrieved
 * passages count toward compaction thresholds exactly like everything else
 * already flowing through `addMessage` (the RAG design doc's context-bloat
 * risk — see `contextTrimmer.test.ts`'s doc-chat coverage). */
export const SOURCES_NOTE_PREFIX = '[Sources]';

/** One retrieval hit inside a `SourcesNotice` — mirrors the fields of
 * `stacks.rs`'s `StackQueryResult` (as returned by the `stacks_query`
 * command) that the citation UI/prompt actually need, renamed to the design
 * doc's `{path, stack, score, snippet}` shape rather than the wire's
 * snake_case `source_path`/`stack_name`/`text` — see
 * `runAgentTurnBody`'s doc-chat block for that mapping. */
export interface SourcesNoticeResult {
  path: string;
  stack: string;
  score: number;
  snippet: string;
}

/** Payload embedded in a sources notice message. */
export interface SourcesNotice {
  results: SourcesNoticeResult[];
}

function isSourcesPayload(value: unknown): value is SourcesNotice {
  const results = (value as SourcesNotice | null)?.results;
  return Boolean(
    value &&
      typeof value === 'object' &&
      Array.isArray(results) &&
      results.every(
        (r): r is SourcesNoticeResult =>
          Boolean(r) &&
          typeof r === 'object' &&
          typeof (r as SourcesNoticeResult).path === 'string' &&
          typeof (r as SourcesNoticeResult).stack === 'string' &&
          typeof (r as SourcesNoticeResult).score === 'number' &&
          typeof (r as SourcesNoticeResult).snippet === 'string',
      ),
  );
}

const sourcesNoticeCodec = makeNotice<SourcesNotice>(SOURCES_NOTE_PREFIX, isSourcesPayload);

export const isSourcesNotice = sourcesNoticeCodec.is;
/** Parses a sources notice's JSON payload; `null` for anything malformed. */
export const parseSourcesNotice = sourcesNoticeCodec.parse;
/** Serializes a sources notice back into message content. */
export const formatSourcesNotice = sourcesNoticeCodec.format;

export const hardenSourcesNoticeForModel = protectKnowledgeNoticeForModel;

/** Prefix identifying a synthetic notice inserted at the start of a session
 * created by `recipeRunner.ts`'s "Run now" (design doc:
 * docs/roadmap/p3-scheduled-automation.md, slice 2) — the 6th notice type
 * anticipated by ROADMAP.md §3.4, and the first to use `makeNotice` from
 * day one rather than being a 6th hand-rolled copy. Marks the session as
 * recipe-originated so `MessageList` can show which recipe (and, once
 * slice 3 ships, whether it was a scheduled run) started it. */
export const RECIPE_NOTE_PREFIX = '[Recipe]';

/** Payload embedded in a recipe notice message. */
export interface RecipeNotice {
  name: string;
  /** Absolute path the recipe was loaded from, if known — omitted for a
   * recipe resolved only by name (the common case; see `recipesStore.ts`). */
  path?: string;
  /** Set only when this run was triggered through a published Local App's
   * `run` route (see `localAppsStore.ts`'s run-request listener) rather than
   * a direct "Run now"/scheduled invocation — the id of that Local App, so
   * `MessageList`/Run Capsule viewers can show which published app started
   * this run. */
  localAppId?: string;
}

function isRecipePayload(value: unknown): value is RecipeNotice {
  return !!value && typeof value === 'object' && typeof (value as RecipeNotice).name === 'string';
}

const recipeNoticeCodec = makeNotice<RecipeNotice>(RECIPE_NOTE_PREFIX, isRecipePayload);

export const isRecipeNotice = recipeNoticeCodec.is;
/** Parses a recipe notice's JSON payload; `null` for anything malformed. */
export const parseRecipeNotice = recipeNoticeCodec.parse;
/** Serializes a recipe notice back into message content. */
export const formatRecipeNotice = recipeNoticeCodec.format;

/**
 * Re-exported for backward compatibility — `isToolCallAllowed` now lives in
 * `turnEngine.ts` (see that module's doc comment) so `subagent.ts`'s own
 * child tool-calling loop can reuse the exact same gate this loop's own
 * dispatch below applies, rather than a parallel/duplicated check. Re-export
 * of the binding already imported above (not a fresh `export ... from`) so
 * this file's own use of it below and the public export are the same value.
 */
export { isToolCallAllowed };

/**
 * Runs one round's worth of model-requested tool calls, splitting `task`
 * calls out to run CONCURRENTLY (bounded by `maxConcurrentSubagents`, the
 * "builds on split-pane turn-safe concurrency" payoff called out in the
 * design doc's Parallelism section — the Rust per-turn `tool_cancel`/
 * permission-`pending` maps and the queued `PermissionModal` were already
 * built for N concurrent turns, not just 2) while every other call stays
 * strictly sequential, exactly as before this feature — a subagent's own
 * tool calls are already serialized within `runSubagentTask`'s own loop, so
 * only concurrency ACROSS multiple `task` calls in the same round is new.
 *
 * `results[i]` always corresponds to `toolCalls[i]` regardless of which
 * call actually finished first — several providers reject a `tool_calls`
 * round trip whose `tool` results don't come back in the same order the
 * calls were requested in, so this ordering guarantee is load-bearing, not
 * cosmetic. `runOne` is a plain callback (not baked in here) so this stays
 * unit-testable with a fake, controllable-timing implementation instead of
 * needing a real `executeToolCall`/Tauri `invoke`.
 */
export async function runToolCallsForRound(
  toolCalls: ToolCall[],
  maxConcurrentSubagents: number,
  runOne: (toolCall: ToolCall) => Promise<string>
): Promise<string[]> {
  const results: string[] = new Array(toolCalls.length);
  const taskIndices: number[] = [];
  const sequentialIndices: number[] = [];
  toolCalls.forEach((toolCall, index) => {
    (toolCall.function.name === 'task' ? taskIndices : sequentialIndices).push(index);
  });

  const sequentialRun = (async () => {
    for (const index of sequentialIndices) {
      results[index] = await runOne(toolCalls[index]);
    }
  })();

  // A small bounded worker pool over `taskIndices` only — `sequentialRun`
  // above already owns every non-`task` index, so these two loops never
  // touch the same slot and can safely run at the same time.
  const poolSize = Math.max(1, Math.min(4, Math.floor(maxConcurrentSubagents) || 1, taskIndices.length || 1));
  let nextTaskCursor = 0;
  const workers = Array.from({ length: poolSize }, async () => {
    while (nextTaskCursor < taskIndices.length) {
      const index = taskIndices[nextTaskCursor++];
      results[index] = await runOne(toolCalls[index]);
    }
  });

  await Promise.all([sequentialRun, ...workers]);
  return results;
}

/** Shape of a successful `tool_remember` result (the created/deduplicated
 * fact) — checked structurally against the tool's stringified result so an
 * error payload (`{ error: string }`) never gets misread as one. */
function parseRememberedFact(resultContent: string): { id: string; text: string } | null {
  try {
    const parsed: unknown = JSON.parse(resultContent);
    if (
      parsed &&
      typeof parsed === 'object' &&
      typeof (parsed as { id?: unknown }).id === 'string' &&
      typeof (parsed as { text?: unknown }).text === 'string'
    ) {
      return parsed as { id: string; text: string };
    }
  } catch {
    // Not JSON — can't be a successful remember result either.
  }
  return null;
}

/** Whether `resultContent` (a `write_file`/`edit_file` tool result string)
 * represents success rather than the `{"error": ...}` shape
 * `stringifyToolError` produces — used only to decide whether to add the
 * call's path to `mutatedFiles` (see `runAgentTurnBody`). Structurally the
 * same check as `MessageList.tsx`'s `resultLooksLikeError`, kept as its own
 * tiny copy here rather than a shared import — both are one-line structural
 * checks against the same wire shape, not enough shared complexity to be
 * worth coupling the two modules over. */
export function isSuccessfulMutationResult(resultContent: string): boolean {
  try {
    const parsed: unknown = JSON.parse(resultContent);
    return !(parsed && typeof parsed === 'object' && 'error' in parsed);
  } catch {
    // Not JSON at all — the plain "Wrote N bytes to …"/"Edited …" success string.
    return true;
  }
}

/** Extracts the `path` argument from a `write_file`/`edit_file` tool call —
 * used only to populate `mutatedFiles`. Never throws: malformed arguments
 * JSON already surfaced as an error result from `executeToolCall` itself, so
 * this just degrades to "no path known" rather than duplicating that error. */
export function toolCallPathArg(toolCall: ToolCall): string | null {
  try {
    const parsed: unknown = JSON.parse(toolCall.function.arguments || '{}');
    const path = (parsed as { path?: unknown } | null)?.path;
    return typeof path === 'string' ? path : null;
  } catch {
    return null;
  }
}

/** Shape returned by the `verify_run` Tauri command — mirrors the Rust
 * `VerifyResult` struct (src-tauri/src/verify.rs) exactly. */
interface VerifyRunResult {
  commandId: string;
  label: string;
  kind: string;
  code: number | null;
  stdout: string;
  stderr: string;
  durationMs: number;
  timedOut: boolean;
}

/** Each verify notice's `output` field is capped at this many chars — a
 * second, wire-facing cap on top of `verify.rs`'s own ~20k-char cap on each
 * of stdout/stderr individually (combining both here can still exceed what's
 * worth sending over IPC and keeping in the transcript). */
const VERIFY_NOTICE_OUTPUT_CAP = 8000;

/** Combines a verify command's stdout/stderr into the single `output` string
 * a `VerifyNotice` carries, tail-capping the combination (a failure's most
 * useful detail is almost always printed last) at
 * `VERIFY_NOTICE_OUTPUT_CAP` chars. */
function buildVerifyOutput(result: VerifyRunResult): string {
  const parts: string[] = [];
  if (result.timedOut) parts.push('Command timed out.');
  if (result.stdout.trim().length > 0) parts.push(result.stdout.trim());
  if (result.stderr.trim().length > 0) parts.push(result.stderr.trim());
  const combined = parts.join('\n\n');
  if (combined.length <= VERIFY_NOTICE_OUTPUT_CAP) return combined;
  return `… (truncated)\n${combined.slice(combined.length - VERIFY_NOTICE_OUTPUT_CAP)}`;
}

/**
 * Runs the workspace's own configured verification commands inside one
 * learning-evaluation sandbox, and reports whether they passed.
 *
 * Same config and same Rust runner the ordinary post-turn phase uses — only
 * the working directory differs, and Rust accepts that directory only for a
 * marker-verified sandbox this app created. `null` means the workspace has no
 * verification configured, which is reported as "no result", never as a pass:
 * an evaluation may not claim a verification that never ran.
 */
export async function runSandboxVerification(
  sandboxPath: string,
  turnId: string,
  signal?: AbortSignal,
  workspacePath?: string,
): Promise<{ passed: boolean; detail: string } | null> {
  let config: VerifyConfig;
  try {
    // The commands of the workspace this sandbox is a copy of, which is not
    // necessarily the one open right now — a candidate learned in A can be
    // evaluated from B, or with nothing open. `verify_run` independently
    // derives the same workspace from the sandbox's own marker, so a wrong
    // value here cannot make it execute another workspace's commands.
    config = await invoke<VerifyConfig>('verify_get_config', { workspacePath });
  } catch {
    return null;
  }
  const enabled = config.commands.filter((command) => command.enabled);
  if (enabled.length === 0) return null;
  for (const command of enabled) {
    if (signal?.aborted) return null;
    try {
      const result = await invoke<VerifyRunResult>('verify_run', {
        commandId: command.id,
        turnId,
        sandboxPath,
      });
      if (result.timedOut || result.code !== 0) {
        return { passed: false, detail: `${result.label}: ${buildVerifyOutput(result).slice(0, 400)}` };
      }
    } catch (err) {
      return { passed: false, detail: `${command.label}: ${errorMessage(err)}` };
    }
  }
  return { passed: true, detail: `${enabled.length} verification command(s) passed` };
}

/** The first failed command from a `runVerificationPhase` pass — enough
 * detail to build the feed-back-to-the-model fix instruction in
 * `runAgentTurnBody`. `null` when every command passed (or none ran). */
export interface VerifyFailure {
  label: string;
  code: number | null;
  output: string;
}

/**
 * Whether a verification failure should trigger one more feed-back round of
 * `runAgentTurnBody`'s tool-calling loop — i.e. whether there's a round left
 * to spend in `settings.verifyMaxRounds`'s budget (default 1, clamp 0-3; 0
 * means report-only, the failure notice is left as-is and never fed back).
 * Extracted out of the loop's exit branch as its own pure function so the
 * round-exhaustion boundary can be exercised directly by tests without
 * mocking the entire turn. A `null` failure (every command passed, or
 * verification didn't run at all — e.g. plan mode, `verifyEnabled` off, no
 * enabled commands) never triggers a round regardless of the round budget.
 */
export function shouldFeedBackVerifyFailure(failure: VerifyFailure | null, verifyRound: number, verifyMaxRounds: number): boolean {
  return failure !== null && verifyRound < verifyMaxRounds;
}

/**
 * Runs every enabled verification command configured for the current
 * workspace (see `verify.rs`/`verifyStore.ts`), in order, appending one
 * `[Verify]` notice per command to the transcript, and returns the first
 * command that failed (if any) so `runAgentTurnBody` can decide whether to
 * feed it back to the model as a fix instruction (bounded by
 * `settings.verifyMaxRounds`). This function itself stays report-only — it
 * never appends a fix-instruction message and never loops — the caller owns
 * all feed-back-round bookkeeping. No-ops (without any IPC calls, returning
 * `null`) unless `verifyEnabled` is on, the active permission mode isn't
 * `'plan'` (belt-and-braces — plan mode already blocks every write, so
 * `mutatedFiles` should already be empty by the time a caller would reach
 * this), and the workspace actually has at least one enabled command
 * configured. `sessionId` is used only to set/clear
 * `sessionStore.runningVerifyLabel` around each command so
 * `MessageList.tsx` can render a "running <label>…" indicator while a
 * (possibly long) command executes — it is not otherwise part of this
 * function's control flow.
 *
 * `signal`, when given, makes Stop work throughout the whole phase, not just
 * the single in-flight command: checked before every command starts (so a
 * Stop that lands between two commands doesn't kick off another one), and
 * raced against each in-flight `verify_run` invoke the same way
 * `executeToolCall` races a tool call — on abort, `tools_cancel_running` is
 * invoked to fire the turn's `tool_cancel` `Notify` (the same channel
 * `run_command_impl`'s `tokio::select!` listens on), so the Rust-side child
 * process actually dies instead of running to completion or its own timeout
 * while the phase — and the turn, and the session's "a turn is already
 * running" guard — sit blocked waiting for it.
 */
export async function runVerificationPhase(
  sessionId: string,
  turnId: string,
  addMessage: (msg: ChatMessage) => void,
  signal?: AbortSignal
): Promise<VerifyFailure | null> {
  if (!useSettingsStore.getState().verifyEnabled) return null;
  if (usePermissionStore.getState().mode === 'plan') return null;
  if (signal?.aborted) return null;

  let config: VerifyConfig;
  try {
    config = await invoke<VerifyConfig>('verify_get_config', {});
  } catch {
    // No workspace open, or the config file couldn't be read — nothing to run.
    return null;
  }

  const enabledCommands = config.commands.filter((c) => c.enabled);
  if (enabledCommands.length === 0) return null;

  let firstFailure: VerifyFailure | null = null;

  for (const cmd of enabledCommands) {
    // Stop fired either before this iteration or while the previous
    // command's invoke was in flight (handled below) — either way, don't
    // start another configured command.
    if (signal?.aborted) break;

    // Surfaced as a "running <label>…" row in the timeline
    // (MessageList.tsx's VerifyRunningRow) — test suites can run long enough
    // (up to `timeout_secs`, default 300s) that a bare typing indicator would
    // read as a hang. Cleared in `finally` so a thrown/rejected invoke below
    // never leaves a stale "running" row behind.
    useSessionStore.getState().setRunningVerifyLabel(sessionId, cmd.label || cmd.command);
    try {
      useUsageHistoryStore.getState().recordVerifyRun();
      const invocation = invoke<VerifyRunResult>('verify_run', { commandId: cmd.id, turnId });
      const result = signal ? await Promise.race([invocation, abortedPromise(signal).then(() => null)]) : await invocation;

      if (result === null) {
        // Aborted mid-command: tell the Rust side to kill it via the same
        // turn-keyed cancel channel `executeToolCall` uses for tool calls,
        // then stop the phase entirely rather than starting the next
        // configured command. The original invocation promise already has a
        // handler attached (via Promise.race), so its eventual (discarded)
        // result never becomes an unhandled rejection.
        void invoke('tools_cancel_running', { turnId }).catch(() => {});
        break;
      }

      const ok = !result.timedOut && result.code === 0;
      const output = buildVerifyOutput(result);
      addMessage({
        role: 'system',
        content: formatVerifyNotice({
          label: result.label,
          kind: result.kind,
          ok,
          code: result.code,
          output,
          durationMs: result.durationMs,
        }),
      });
      if (!ok && firstFailure === null) {
        firstFailure = { label: result.label, code: result.code, output };
      }
    } catch (err) {
      // verify_run itself rejected (e.g. the command was deleted from the
      // config in another window between the check above and this call) —
      // surface it as a failed notice rather than silently dropping the
      // round.
      const message = errorMessage(err);
      addMessage({
        role: 'system',
        content: formatVerifyNotice({
          label: cmd.label,
          kind: cmd.kind,
          ok: false,
          code: null,
          output: message,
          durationMs: 0,
        }),
      });
      if (firstFailure === null) {
        firstFailure = { label: cmd.label, code: null, output: message };
      }
    } finally {
      useSessionStore.getState().setRunningVerifyLabel(sessionId, null);
    }
  }

  return firstFailure;
}




/** Deterministic `/compact` entry point. It reuses the same compaction and
 * one-shot summary path as automatic context trimming, but never appends a
 * user/model turn. The persisted transcript is replaced only after a full
 * compacted result has been produced. */
export async function compactSessionNow(sessionId: string): Promise<{ changed: boolean; removedMessages: number }> {
  const sessionState = useSessionStore.getState();
  if (sessionState.runningTurns[sessionId]) {
    throw new Error("Stop the active turn before compacting this chat.");
  }
  const history = sessionMessages(sessionId);
  if (history.length === 0) return { changed: false, removedMessages: 0 };

  const settings = useSettingsStore.getState();
  const privacyWorkspaceId =
    primaryRoot(useWorkspaceStore.getState().roots)?.path ?? 'global';
  const privacyWireCache: PrivacyWireCache = new Map();
  // Summarization is its own K9 task class: it is bulk, throwaway work that a
  // user may well want on a cheaper or local model than the conversation
  // itself. It offers no tools and never carries an image. Not applied to the
  // global active target — compacting a chat must not change what the next
  // real turn runs on.
  let target = routeFromActive(await resolveTarget(), {
    taskClass: 'summarize',
    requiresVision: false,
    requiresTools: false,
  }).target;
  const result = await applyContextCompaction(history, {
    strategy: settings.contextTrimStrategy,
    contextLimit: useUsageStore.getState().contextLimit,
    thresholdPercent: 100,
    sendForSummary: async (dropped) => {
      let summaryMessages: ChatMessage[] = [
        {
          role: 'system',
          content:
            'Summarize the following earlier conversation concisely for another AI assistant to continue from. Preserve key facts, decisions, file paths, and code context. Reply with only the summary text.',
        },
        { role: 'user', content: renderForSummary(dropped) },
      ];
      if (target.kind === 'provider') {
        const gated = await gatePrivacyWireMessages(
          summaryMessages,
          (content) =>
            usePrivacyFirewallStore
              .getState()
              .gateOutbound(content, 'cloud_model', privacyWorkspaceId),
          privacyWireCache,
        );
        if (gated.action === 'cancelled') {
          throw new Error('Privacy Firewall cancelled cloud summarization before any content was sent.');
        }
        if (gated.action === 'switch_local') {
          const local = findLocalOnlyTarget();
          if (!local) {
            throw new Error('Privacy Firewall requested local summarization, but no genuinely local Ollama model is configured.');
          }
          target = local;
          applyTargetSwitch(local);
          useSessionStore.getState().addMessage(sessionId, {
            role: 'system',
            content: `${PRIVACY_NOTE_PREFIX} Switched manual compaction to ${targetLabel(local)} before protected history could leave the machine.`,
          });
        } else {
          summaryMessages = gated.messages;
          if (gated.newlyRedacted.length > 0) {
            useSessionStore.getState().addMessage(sessionId, {
              role: 'system',
              content: `${PRIVACY_NOTE_PREFIX} Redacted ${gated.newlyRedacted.length} sensitive item(s) before cloud summarization: ${describeRedactions(gated.newlyRedacted)}.`,
            });
          }
        }
      }
      const summary = await attemptStream(
        target,
        summaryMessages,
        [],
        undefined,
        effortForTarget(target),
        sessionId,
        undefined,
        true,
        undefined,
        undefined,
        true,
        { preGated: true },
      );
      if (summary.streamError) throw new Error(summary.streamError);
      return summary.content.trim() || '(summary unavailable)';
    },
  });
  if (!result.changed) return { changed: false, removedMessages: 0 };
  useSessionStore.getState().replaceMessages(sessionId, result.messages);
  return {
    changed: true,
    removedMessages: Math.max(0, history.length - result.messages.length + 1),
  };
}






/**
 * A conversation can carry an image from an earlier turn (sent to a
 * vision-capable model) into a later turn where the user has since switched
 * to a text-only model — `requireVision` below only accounts for THIS turn's
 * *new* attachments, not images already baked into stored history, so
 * without this the text-only model's provider rejects the whole request over
 * a `ChatContentPart[]` it can't parse (image content from turns ago). Called
 * per-target (not just once) since a mid-turn failover can move between
 * vision and non-vision targets. A no-op (returns `messages` unchanged) for
 * a target that does support vision. The image bytes themselves are never
 * recoverable here — a text-only model genuinely cannot see them regardless
 * — but a bare `textContent()` call would also erase every *trace* that an
 * image was ever attached, leaving the model to silently misread "what's in
 * that image?" a few turns later as referring to nothing. A short marker is
 * appended per stripped message instead, so the model at least knows why it
 * can't answer.
 */
function stripImagesForTextOnlyTarget(messages: ChatMessage[], target: ResolvedTarget): ChatMessage[] {
  if (resolvedTargetSupportsVision(target)) return messages;
  return messages.map((message) => {
    if (!Array.isArray(message.content)) return message;
    const imageCount = message.content.filter((part) => part.type === 'image_url').length;
    const text = textContent(message.content);
    if (imageCount === 0) return { ...message, content: text };
    const marker = `[${imageCount} image${imageCount > 1 ? 's' : ''} attached here — not visible to the current model]`;
    return { ...message, content: text.length > 0 ? `${text}\n\n${marker}` : marker };
  });
}

/** Whether the currently active target satisfies `requireVision` (always `true` when vision isn't required). Local llama.cpp vision capability is delegated to `visionModels.ts`'s single source of truth. */
function activeTargetSatisfiesVision(requireVision: boolean): boolean {
  if (!requireVision) return true;
  const state = useModelStore.getState();
  if (state.activeProvider === 'provider') {
    if (!state.activeProviderId || !state.activeProviderModel) return false;
    return isVisionCapableProviderModel(state.activeProviderId, state.activeProviderModel);
  }
  if (state.activeProvider === 'ollama') {
    const model = state.ollamaModels.find((m) => m.name === state.activeOllamaModel);
    return model ? isVisionCapableOllamaModel(model) : false;
  }
  return isVisionCapableLocalModel();
}

/**
 * Builds the cloud-provider failover/vision-switch chain: the currently
 * active provider target first (if it qualifies), then every other
 * has-key provider with a cached model list, each represented by its
 * `lastModelForProvider` pick (or first cached model) — filtered to a
 * vision-capable model of that provider when `requireVision` is set.
 * Local llama.cpp and Ollama deliberately never appear here: "try another
 * free-tier provider" doesn't apply to a single local machine, so only
 * cloud providers participate in this chain (Ollama can still be the
 * *primary* target, and still participates in vision-switch search — see
 * `findVisionCandidate` — just not in this error-driven chain).
 */
function buildFailoverChain(requireVision: boolean): ResolvedTarget[] {
  const state = useModelStore.getState();
  const chain: ResolvedTarget[] = [];

  if (state.activeProvider === 'provider' && state.activeProviderId && state.activeProviderModel) {
    if (!requireVision || isVisionCapableProviderModel(state.activeProviderId, state.activeProviderModel)) {
      chain.push({ kind: 'provider', providerId: state.activeProviderId, model: state.activeProviderModel });
    }
  }

  for (const provider of state.providers) {
    if (!provider.has_key) continue;
    if (chain.some((c) => c.kind === 'provider' && c.providerId === provider.id)) continue;

    const models = state.providerModels[provider.id] ?? [];
    if (models.length === 0) continue;

    if (!requireVision) {
      const preferred = state.lastModelForProvider[provider.id];
      const modelId = preferred && models.some((m) => m.id === preferred) ? preferred : models[0].id;
      chain.push({ kind: 'provider', providerId: provider.id, model: modelId });
      continue;
    }

    const preferred = state.lastModelForProvider[provider.id];
    const preferredIsVision = preferred && isVisionCapableProviderModel(provider.id, preferred);
    const visionModel = preferredIsVision ? preferred : models.find((m) => isVisionCapableProviderModel(provider.id, m.id))?.id;
    if (visionModel) chain.push({ kind: 'provider', providerId: provider.id, model: visionModel });
  }

  return chain;
}

/**
 * The Privacy Firewall's (ROADMAP.md Phase 5) "switch to local" fallback —
 * the currently selected Ollama model if one is configured, otherwise its
 * first cached model. Deliberately never managed llama.cpp: starting that
 * requires an explicit, already-downloaded model and an `llama_start` call
 * (see `modelStore.ts::start`) this gate has no basis to make unattended,
 * the same reasoning `buildFailoverChain`'s doc comment gives for why local
 * llama.cpp never appears in *that* automatic switch chain either. Returns
 * `null` when no Ollama model is configured at all, in which case the
 * caller cancels the turn rather than guessing at a target.
 */
function findLocalOnlyTarget(): ResolvedTarget | null {
  const state = useModelStore.getState();
  const localModels = state.ollamaModels.filter((model) => model.is_cloud !== true);
  const preferred =
    state.activeOllamaModel && localModels.some((model) => model.name === state.activeOllamaModel)
      ? state.activeOllamaModel
      : localModels[0]?.name;
  if (!preferred) return null;
  return { kind: 'ollama', baseUrl: 'http://127.0.0.1:11434', model: preferred };
}

/** Searches every configured target (cloud providers first, then Ollama) for one that can see images, for the pre-turn vision auto-switch. Returns `null` if nothing qualifies. */
function findVisionCandidate(): ResolvedTarget | null {
  const chain = buildFailoverChain(true);
  if (chain.length > 0) return chain[0];

  const visionOllama = useModelStore.getState().ollamaModels.find(isVisionCapableOllamaModel);
  if (visionOllama) return { kind: 'ollama', baseUrl: 'http://127.0.0.1:11434', model: visionOllama.name };

  return null;
}

/** Prefix identifying a synthetic dispatch-policy notice (K9 — see
 * `modelRouting.ts`). Reuses `SWITCH_NOTE_PREFIX` rather than minting a fourth
 * prefix: a routed target IS a model switch as far as the transcript and
 * `MessageList`'s rendering are concerned, and the sentence names the policy
 * that caused it. */
export const ROUTING_NOTE_PREFIX = SWITCH_NOTE_PREFIX;







/** An explicit attachment (from the "+" attach menu), as opposed to a text-derived "@"-mention. */
export interface AttachmentRef {
  path: string;
  isDir: boolean;
  /** Set at pick-time in `ChatWindow.tsx` for image files — its presence (alongside `dataUrl`) is what routes this attachment to the vision content-part path instead of the text-inlining path below. */
  kind?: 'image' | 'inline_text';
  /** The already-base64-encoded `data:` URL for an image attachment, read once at pick-time (see `imageAttachment.ts`) so this module never re-reads the file. */
  dataUrl?: string;
  /** Bounded, user-approved inline text. Currently used by terminal evidence
   * so it never needs to masquerade as a readable workspace path. */
  content?: string;
  /** Optional chip label for virtual attachments such as terminal evidence. */
  label?: string;
}

/** A single resolved image attachment, ready to become a `ChatContentPart`. */
export interface ResolvedImage {
  path: string;
  dataUrl: string;
}

/**
 * Resolves every "@"-mentioned path in `text`, merged with any explicit
 * non-image `attachments` (deduplicated by path — an attachment's `isDir`
 * flag wins over a text mention for the same path), into file content or a
 * directory listing via `tool_list_dir`/`tool_read_file`. Image attachments
 * are split off separately: they already carry a pre-encoded `dataUrl` from
 * `ChatWindow.tsx`'s pick-time read, so they never touch these Tauri
 * commands. A text reference that fails to resolve (doesn't exist,
 * permission error, etc.) is skipped — left as plain text in what the model
 * sees — but its path is collected into `unresolved` so the caller can
 * surface a notice instead of the failure staying invisible. Resolution
 * failure never fails the turn.
 */
export async function resolveReferences(
  text: string,
  attachments: AttachmentRef[]
): Promise<{ textRefs: ResolvedTextReference[]; images: ResolvedImage[]; unresolved: string[] }> {
  const images: ResolvedImage[] = [];
  const textAttachments: AttachmentRef[] = [];
  const textRefs: ResolvedTextReference[] = [];
  const inlinePaths = new Set<string>();

  for (const attachment of attachments) {
    if (attachment.kind === 'image') {
      if (attachment.dataUrl) images.push({ path: attachment.path, dataUrl: attachment.dataUrl });
      continue;
    }
    if (attachment.kind === 'inline_text') {
      if (attachment.content && !inlinePaths.has(attachment.path)) {
        inlinePaths.add(attachment.path);
        textRefs.push({
          path: attachment.path,
          isDir: false,
          content: attachment.content,
          source: 'terminal',
        });
      }
      continue;
    }
    textAttachments.push(attachment);
  }

  const merged = new Map<string, boolean>();
  for (const path of extractMentionPaths(text)) {
    if (!merged.has(path)) merged.set(path, false);
  }
  for (const attachment of textAttachments) {
    merged.set(attachment.path, attachment.isDir);
  }

  const unresolved: string[] = [];

  for (const [path, isDir] of merged) {
    if (isDir) {
      try {
        const entries = await invoke<DirEntry[]>('tool_list_dir', { path });
        textRefs.push({ path, isDir: true, content: formatDirListing(entries) });
      } catch {
        // Not a directory, doesn't exist, or unreadable — skip it.
        unresolved.push(path);
      }
    } else {
      try {
        const content = await invoke<string>('tool_read_file', { path });
        textRefs.push({ path, isDir: false, content });
      } catch {
        // Not a file, doesn't exist, or unreadable — skip this mention.
        unresolved.push(path);
      }
    }
  }

  return { textRefs, images, unresolved };
}

/**
 * Combines `text` with any resolved images into the shape a `ChatMessage`'s
 * `content` should actually be: a plain string when there are no images
 * (the overwhelming majority of messages), or a `ChatContentPart[]` (one
 * `text` part followed by one `image_url` part per image) when there's at
 * least one. Used for both what gets *stored* in the session (so the chat
 * UI can render the attached image(s), not just send them) and, when text
 * references also need expanding, what gets substituted into the *wire*
 * payload — see `runAgentTurn`.
 */
export function toMessageContent(text: string, images: ResolvedImage[]): string | ChatContentPart[] {
  if (images.length === 0) return text;
  const parts: ChatContentPart[] = [{ type: 'text', text }];
  for (const image of images) parts.push({ type: 'image_url', image_url: { url: image.dataUrl } });
  return parts;
}

/**
 * Runs one full agentic turn for `userText` in session `sessionId` (the
 * chat pane that submitted it — with the split pane open, two turns can run
 * concurrently in different sessions): appends it to that session as a
 * user message, then repeatedly calls the active model with the full
 * history and the available tools, streaming its reply into the chat and
 * executing any requested tool calls, until the model answers without
 * requesting further tools or the safety cap is reached.
 *
 * Before the first network attempt, this also (in order): checks whether an
 * attached image needs a vision-capable model and switches to one if so
 * (see `findVisionCandidate`), and builds the ordered attempt sequence used
 * for auto-failover (see `buildFailoverChain`) — both governed by
 * `settingsStore` toggles. Each iteration of the tool-calling loop also
 * checks whether history has crossed the configured context-trim threshold
 * and compacts it in place if so (see `contextTrimmer.ts`).
 */
/** In-flight turns' abort handles, keyed by session id. The turn — not the
 * pane — owns its controller: a pane switch mid-turn must leave the turn
 * stoppable from whichever pane shows its session later. */
const turnControllers = new Map<string, AbortController>();
const externallyRequestedCancellations = new Set<string>();
const cancellationDisposers = new Map<AbortController, Array<() => void>>();
/** This turn's process-table id, keyed by turn id — set once `admitProcess`
 * resolves in `runAgentTurn`, read by `runAgentTurnBody`'s round-boundary
 * pause checks so `honourPause` can mark the process `suspended`/`running`
 * around the wait. */
const chatTurnProcesses = new Map<string, string>();

/** Registers `runId` as cancellable through `controller`, and — same call,
 * same lifetime — as belonging to `sessionId`, so a permission prompt
 * arriving from Rust under that id can be attributed to the conversation it
 * blocks (`sessionStore.turnSessions`). The two are registered together
 * because they answer the same question about the same ids: every id a
 * durable run can be cancelled by is an id a prompt can arrive under —
 * this turn's own, a daemon run's, a failover's fresh recorder run. Both
 * are released by the same disposer list in `runAgentTurn`'s finally. */
function registerDurableController(
  runId: string,
  controller: AbortController,
  sessionId: string,
): void {
  const dispose = registerRunCancellation(runId, () => {
    externallyRequestedCancellations.add(runId);
    controller.abort();
  });
  useSessionStore.getState().markTurnSession(runId, sessionId);
  const existing = cancellationDisposers.get(controller) ?? [];
  existing.push(dispose, () => useSessionStore.getState().markTurnSession(runId, null));
  cancellationDisposers.set(controller, existing);
}

interface DurableTurnContext {
  recorder: DurableRunRecorder | null;
  failure: string | null;
  /** One-shot model call for the bounded learning reflection pass, built by
   * `runAgentTurnBody` (which owns the resolved target and the privacy gate)
   * and consumed by `runAgentTurn`'s `finally` — the learning step can only
   * run once the durable run is COMPLETE, because the backend classifies the
   * signal from that run's own terminal events. `null` when the turn never
   * got as far as resolving a target. */
  reflect: ReflectionCall | null;
  /** This turn's live skill context. Held by reference (not a copy) because
   * `invokedCommands` is mutated in place as the turn runs, so
   * `runAgentTurn`'s `finally` sees every skill the turn ended up using no
   * matter which of this loop's early returns it took. */
  skills: SkillToolContext | null;
  /** Failing tool results this turn produced, classified exactly as the
   * durable recorder classifies them. Part of a learned skill's effectiveness
   * record, and the reason a turn that "completed" with three failed tool
   * calls is not counted as a success for the skill it used. */
  toolFailures: string[];
}

/** Cap on the failing tool results carried into a learned skill's
 * effectiveness record — the record is a signal, not a log. */
const MAX_RECORDED_TOOL_FAILURES = 8;

/**
 * One invoked native skill, paired with the exact content hash it was frozen
 * at. Local prompt skills and signed packages are excluded: neither can be a
 * learned skill, and neither carries a content hash an outcome could be
 * attributed to.
 *
 * Scope comes from the descriptor id `nativeSkills` built (`native:<scope>:…`)
 * rather than from a second lookup, so it always agrees with the root the
 * skill was actually discovered in.
 */
export function skillUse(skill: SlashSkill): InvokedSkillUse | null {
  if (skill.source !== 'native') return null;
  const scope = skill.id.split(':')[1];
  if (scope !== 'global' && scope !== 'workspace') return null;
  return { command: skill.command, scope, sha256: skill.contentSha256 };
}

/** Writes the durable `skill_invoked` event for one invocation. The run's own
 * record of WHICH version it ran — the only thing a later effectiveness,
 * correction or regression judgement can honestly be keyed to, since the
 * installed version can have moved on (or been rolled back) by then. */
function recordSkillInvocation(durable: DurableTurnContext, skill: SlashSkill): void {
  const use = skillUse(skill);
  if (!use) return;
  durable.recorder?.recordSkillInvoked(use.command, use.scope, use.sha256);
}

/** Aborts the in-flight turn for `sessionId`, if any. The panes' Stop
 * buttons call this instead of holding their own AbortController — see
 * `turnControllers`. */
export function stopTurn(sessionId: string): void {
  turnControllers.get(sessionId)?.abort();
}

export async function runAgentTurn(
  sessionId: string,
  userText: string,
  attachments: AttachmentRef[] = [],
  signal?: AbortSignal,
  // Caller-supplied turn id, in place of the `crypto.randomUUID()` this
  // function generates internally by default. The one real consumer is
  // `recipeRunner.ts`: it needs the id *before* the turn starts so it can
  // call `permissions.rs`'s `set_permission_mode_for_turn` ahead of time
  // (and `clear_permission_mode_for_turn` once `done` settles) instead of
  // flipping the app's single global mode for the run's duration — see
  // `RecipeRunHandle`'s doc comment. Every other caller omits this and gets
  // an internally-generated id, unchanged from before.
  turnIdOverride?: string,
  skillInvocations: SkillInvocationSnapshot[] = [],
  // Every skill installed/enabled for this session, not just the ones
  // `skillInvocations` already explicitly invoked — lets the (opt-in, see
  // `settingsStore.skillAutoInvokeEnabled`) `skill` tool auto-invoke any of
  // the rest. Only threaded through to `runTurnGuarded`'s in-process loop —
  // the daemon path (`runDaemonAgentTurn`) has its own independent
  // Rust-side tool composition (see `daemonDesktopTurn.ts`'s `tool_profile`)
  // and isn't part of this feature yet, so it's simply never passed there.
  // Default `[]` mirrors `skillInvocations`'s own default, so every other
  // caller (`PlanCard.tsx`, `recipeRunner.ts`) is unaffected.
  availableSkills: SlashSkill[] = [],
  // Ultracode (the Effort slider's trailing stop, see `EffortSelector.tsx`):
  // same model, same single turn, but the system prompt gains
  // `ULTRACODE_SYSTEM_SECTION` and the `task` tool is force-offered — a
  // standing opt-in for multi-agent orchestration, mirroring Claude Code's
  // "ultracode" keyword. In-process loop only; the daemon path composes its
  // own Rust-side prompt/tools and isn't part of this feature yet, same
  // stance as `availableSkills` above.
  ultracode = false,
  // Set only by `frozenTurn.ts`: this call is not a new question, it is the
  // continuation of a turn that was frozen at a tool boundary and is being
  // re-entered from its image. It suppresses the user message this function
  // otherwise appends — the conversation is already whole, and a blank one would
  // be a turn the user never took — and writes the determinism caveats into the
  // transcript, because whoever reads the continuation is the person who needs
  // to know what a resume does not reproduce.
  resume: ResumedTurn | null = null,
  // Which of the operator's own surfaces this turn was made on. `voice` is a
  // finalized hands-free utterance the companion overlay auto-sent; everything
  // else is the composer. Both are the operator, both take the same durable
  // ingress path, and the value only decides how the turn is labelled in the
  // ingress listing — see `ConversationSource` on the Rust side.
  origin: DesktopTurnSource = 'desktop',
): Promise<void> {
  // Hard invariant: at most one turn per session, ever. Two turns streaming
  // into one transcript interleave their `updateLastMessage` patches and
  // corrupt it — the store's pane guards make this unreachable through the
  // UI, but the loop enforces it regardless of caller.
  if (turnControllers.has(sessionId)) {
    throw new Error('A turn is already running in this session.');
  }
  const controller = new AbortController();
  const turnId = turnIdOverride ?? crypto.randomUUID();
  if (signal) {
    if (signal.aborted) controller.abort();
    else signal.addEventListener('abort', () => controller.abort(), { once: true });
  }
  turnControllers.set(sessionId, controller);
  registerDurableController(turnId, controller, sessionId);
  useSessionStore.getState().markTurnRunning(sessionId, true);
  useTurnStatusStore.getState().begin(sessionId);
  const startedAt = Date.now();
  // Project this turn onto the unified process table so it is visible alongside
  // daemon jobs, subagents and workflow runs. Fail-soft by construction — see
  // `processTable.ts`.
  const processId = await admitProcess({
    kind: 'chat_turn',
    externalId: turnId,
    workspace: primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null,
    profile:
      useSessionStore.getState().sessions.find((entry) => entry.id === sessionId)?.personaId ??
      null,
  });
  if (processId) {
    await markProcessRunning(processId);
    chatTurnProcesses.set(turnId, processId);
  }
  let turnError: unknown;
  try {
    // A resume is settled before it gets here: the backend already holds the
    // continuation, so there is nothing left to route, gate or preflight. Asking
    // `daemonDesktopRoute` again would only add a way for an accepted
    // continuation to go unwatched because the runner's health changed in the
    // second between accepting it and attaching to it.
    if (resume !== null) {
      await watchResumedDesktopTurn(sessionId, resume, controller, origin);
      return;
    }
    const mutationRequired = requiresWorkspaceMutation(
      userText,
      usePermissionStore.getState().mode,
    );
    const sessionState = useSessionStore.getState();
    const activeWorkspacePath = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
    const session = sessionState.sessions.find((entry) => entry.id === sessionId);
    // A blank compose slate may have been created before the user selected a
    // different folder. Bind that still-empty session to the folder active at
    // its first turn; once a transcript exists, the binding is immutable and
    // the mismatch preflight below protects against editing another project.
    if (
      session
      && session.messages.length === 0
      && activeWorkspacePath !== null
      && session.workspacePath !== activeWorkspacePath
    ) {
      useSessionStore.setState((state) => ({
        sessions: state.sessions.map((entry) =>
          entry.id === sessionId ? { ...entry, workspacePath: activeWorkspacePath } : entry
        ),
      }));
    }
    const sessionWorkspacePath =
      session?.messages.length === 0 ? activeWorkspacePath : session?.workspacePath ?? null;
    const preflightFailure = workspaceMutationPreflightFailure(
      mutationRequired,
      activeWorkspacePath,
      sessionWorkspacePath,
    );
    if (preflightFailure !== null) {
      sessionState.addMessage(sessionId, { role: 'user', content: userText });
      sessionState.addMessage(sessionId, { role: 'assistant', content: preflightFailure });
      return;
    }

    const route = await daemonDesktopRoute();
    // Every conversational turn this surface accepts becomes a durable ingress
    // turn before any agent runs — whatever the turn is.
    //
    // There used to be two exceptions here, and both were execution bypasses
    // rather than features. A turn classified as workspace-mutating stayed local
    // because only this loop could tell whether a write happened; that proof now
    // comes from the runtime, and the corrective attempt it justified is a
    // durable continuation the backend owns (`workspace_mutation_required` in
    // `daemonDesktopTurn.ts`, and `channels::mutation` on the Rust side). A
    // resumed turn stayed local because its image was written here; a resume is
    // now a durable continuation of the accepted turn the image belongs to,
    // executed from the context frozen when that turn was accepted rather than
    // from whatever this process is configured with at resume time.
    //
    // `browser` is not an exception to that rule either: it is the one
    // configuration in which no durable execution authority can exist on the
    // machine at all — a dev profile with no Tauri bridge. There is nothing to
    // hand the turn to there, so the in-process loop is the only thing that can
    // run it. A packaged desktop never reaches it: `daemonDesktopRoute` refuses
    // rather than routing a turn away from a runner that is missing, stopped,
    // stale or kill-switched, and `runTurnGuarded` refuses again on its own.
    if (route === 'daemon') {
      await runDaemonAgentTurn(
        sessionId,
        userText,
        attachments,
        controller.signal,
        turnId,
        skillInvocations,
        origin,
        mutationRequired,
      );
    } else {
      await runTurnGuarded(
        sessionId,
        userText,
        attachments,
        controller.signal,
        turnId,
        skillInvocations,
        availableSkills,
        ultracode,
        mutationRequired,
        route,
      );
    }
  } catch (error) {
    turnError = error;
    throw error;
  } finally {
    turnControllers.delete(sessionId);
    cancellationDisposers.get(controller)?.forEach((dispose) => dispose());
    cancellationDisposers.delete(controller);
    externallyRequestedCancellations.delete(turnId);
    chatTurnProcesses.delete(turnId);
    forgetPause(turnId);
    useSessionStore.getState().markTurnRunning(sessionId, false);
    // Badge the sidebar row for a session the user has navigated away from.
    // A cancelled turn records nothing: the user pressed Stop, so there is
    // no outcome to go back and look at.
    if (!controller.signal.aborted) {
      useSessionStore.getState().noteTurnOutcome(sessionId, turnError ? 'error' : 'done');
    }
    useTurnStatusStore.getState().end(sessionId);
    useUsageHistoryStore.getState().recordTurnCompleted(Date.now() - startedAt);
    if (processId) {
      const outcome = exitStatusFor({
        aborted: controller.signal.aborted,
        error: turnError,
      });
      await exitProcess(processId, outcome.status, outcome.reason);
    }
  }
}

function daemonProjectionContent(
  projection: Awaited<ReturnType<typeof watchDaemonDesktopTurn>>,
): string {
  if (!projection.terminal) {
    return projection.output || `⏳ ${projection.status}`;
  }
  if (projection.terminalStatus === 'succeeded') {
    return projection.output || projection.summary || 'Background turn completed.';
  }
  if (projection.terminalStatus === 'cancelled') {
    return projection.output || projection.status || 'Background turn stopped.';
  }
  const error = projection.error || projection.status || 'The resident agent did not complete the turn.';
  return projection.output ? `${projection.output}\n\n[Background run error: ${error}]` : `[Background run error: ${error}]`;
}

async function attachDaemonTurnToChat(
  link: ActiveDaemonDesktopTurn,
  controller: AbortController,
): Promise<void> {
  let latestContent = link.output || '⏳ Reconnecting to the resident runner…';
  let terminal = false;
  try {
    const projection = await watchDaemonDesktopTurn(link, controller.signal, {
      onLinkChanged: saveActiveDaemonTurn,
      onProjection: (next) => {
        latestContent = daemonProjectionContent(next);
        useSessionStore.getState().updateMessageAt(link.sessionId, link.assistantIndex, {
          content: latestContent,
        });
      },
    });
    terminal = projection.terminal;
    latestContent = daemonProjectionContent(projection);
    useSessionStore.getState().updateMessageAt(link.sessionId, link.assistantIndex, {
      content: latestContent,
    });
  } catch (error) {
    const message = errorMessage(error);
    useSessionStore.getState().updateMessageAt(link.sessionId, link.assistantIndex, {
      content: link.output
        ? `${link.output}\n\n[Lost connection to resident run: ${message}]`
        : `[Lost connection to resident run: ${message}]`,
    });
    // Keep the link: a later app start can replay all durable events after
    // the last committed sequence instead of treating a transient IPC/SQLite
    // failure as terminal.
    throw error;
  } finally {
    // Only terminal runs are removed by the successful path. A stopped UI
    // controller can return before the daemon commits Cancelled, so preserve
    // its link for startup recovery in that case.
    if (terminal) removeActiveDaemonTurn(link.runId);
  }
}

/**
 * Attach the transcript to a resumed turn the backend has already accepted.
 *
 * Nothing here resolves configuration, nothing here executes, and — since the
 * lifecycle was straightened out — nothing here *submits* either. The
 * continuation was accepted before its caller retired anything, because a turn
 * this loop had to reach the backend for would be a turn whose frozen image had
 * to be destroyed first to know whether the resume worked. So the submission
 * belongs to `frozenTurn.ts`, which owns the image, and what is left here is
 * watching: the run inherits the execution context frozen when the *parent* turn
 * was accepted — the recipe, model, workspace and permission mode of then, not
 * of now — and this process only learns which run that is.
 */
async function watchResumedDesktopTurn(
  sessionId: string,
  resume: ResumedTurn,
  controller: AbortController,
  origin: DesktopTurnSource,
): Promise<void> {
  const store = useSessionStore.getState();
  // The caveats are the whole reason `checkpoint_restorability` returns them: a
  // resumed turn is a fresh generation from the frozen point, not a replay, and
  // the transcript is where the person reading the continuation will be.
  store.addMessage(sessionId, {
    role: 'system',
    content: [`${RESUME_NOTE_PREFIX} Resumed from a frozen image.`, ...resume.determinismCaveats].join('\n'),
  });
  void reconcileProcess({
    kind: 'daemon_job',
    externalId: resume.accepted.jobId,
    state: 'admitted',
    parentKind: 'chat_turn',
    parentExternalId: resume.parentTurnId,
  });
  const assistantIndex = sessionMessages(sessionId).length;
  store.addMessage(sessionId, { role: 'assistant', content: '⏳ Continuing the frozen turn…' });
  const link: ActiveDaemonDesktopTurn = {
    sessionId,
    // The continuation's own identity is the backend's; what this link needs is
    // the accepted turn it belongs to, so a reconnect asks about the same turn.
    turnId: resume.parentTurnId,
    runId: resume.accepted.runId,
    assistantIndex,
    lastSequence: 0,
    output: '',
    source: origin,
  };
  saveActiveDaemonTurn(link);
  registerDurableController(resume.accepted.runId, controller, sessionId);
  await attachDaemonTurnToChat(link, controller);
}

async function runDaemonAgentTurn(
  sessionId: string,
  userText: string,
  attachments: AttachmentRef[],
  signal: AbortSignal,
  turnId: string,
  skillInvocations: SkillInvocationSnapshot[],
  origin: DesktopTurnSource,
  mutationRequired: boolean,
): Promise<void> {
  const store = useSessionStore.getState();
  const priorMessages = sessionMessages(sessionId);
  const anchorIndex = priorMessages.length;
  store.addMessage(sessionId, { role: 'user', content: userText });

  const { textRefs, images, unresolved } = await resolveReferences(userText, attachments);
  if (images.length > 0) {
    store.updateMessageAt(sessionId, anchorIndex, { content: toMessageContent(userText, images) });
  }
  if (signal.aborted) throw new DOMException('Turn cancelled', 'AbortError');

  const settings = useSettingsStore.getState();
  if (!activeTargetSatisfiesVision(images.length > 0) && settings.autoVisionSwitchEnabled) {
    const candidate = findVisionCandidate();
    if (candidate) applyTargetSwitch(candidate);
  }
  // Same K9 dispatch policy as the local turn path, and in the same position:
  // after the vision auto-switch, before the Privacy Firewall gate below.
  const routed = routeFromActive(await resolveTarget(), {
    taskClass: 'chat',
    requiresVision: images.length > 0,
    requiresTools: true,
  });
  let resolvedTarget = routed.target;
  if (routed.decision.changedFromActive) {
    applyTargetSwitch(resolvedTarget);
    store.addMessage(sessionId, {
      role: 'system',
      content: `${ROUTING_NOTE_PREFIX} ${routed.decision.reason}`,
    });
  }
  let targetSnapshot = snapshotForResolvedTarget(resolvedTarget);
  if (!targetSnapshot) {
    throw new Error('The selected model target could not be frozen for the resident runner.');
  }

  await useRulesStore.getState().refresh();
  const session = useSessionStore.getState().sessions.find((entry) => entry.id === sessionId);
  const attachedStackIds = session?.attachedStackIds ?? [];
  const attachedStacks = attachedStackIds.length > 0
    ? useStackStore.getState().stacks.filter((stack) => attachedStackIds.includes(stack.id))
    : [];
  const attachedStacksForPrompt = attachedStackPromptInfo(attachedStacks);
  const docChatMode = session?.docChatMode ?? false;
  let sourcesNotice: string | null = null;
  if (docChatMode && attachedStackIds.length > 0) {
    try {
      const hits = await invoke<StackQueryResult[]>('stacks_query', {
        stackIds: attachedStackIds,
        query: userText,
      });
      if (hits.length > 0) {
        sourcesNotice = formatSourcesNotice({
          results: hits.map((hit) => ({
            path: hit.source_path,
            stack: hit.stack_name,
            score: hit.score,
            snippet: hit.text,
          })),
        });
      }
    } catch {
      // Same best-effort semantics as the local turn path.
    }
  }

  const composedText = composeReferencedText(userText, textRefs);
  const wireCurrent: ChatMessage = {
    role: 'user',
    content: toMessageContent(composedText, images),
  };
  const history: ChatMessage[] = [
    ...priorMessages,
    ...(sourcesNotice ? [{ role: 'system' as const, content: sourcesNotice }] : []),
    wireCurrent,
  ];
  let targetHistory = stripImagesForTextOnlyTarget(history, resolvedTarget);
  let frozenTextRefs = textRefs;
  if (resolvedTarget.kind === 'provider') {
    const historyLength = targetHistory.length;
    const privacyMessages: ChatMessage[] = [
      ...targetHistory,
      ...textRefs.map((reference) => ({
        role: 'user' as const,
        content: reference.content,
      })),
    ];
    const privacyOutcome = await gatePrivacyWireMessages(
      privacyMessages,
      (content) =>
        usePrivacyFirewallStore
          .getState()
          .gateOutbound(
            content,
            'cloud_model',
            primaryRoot(useWorkspaceStore.getState().roots)?.path ?? 'global',
          ),
      new Map(),
    );
    if (privacyOutcome.action === 'cancelled') {
      store.addMessage(sessionId, {
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} The resident turn was cancelled before protected content could be submitted to ${targetLabel(resolvedTarget)}.`,
      });
      return;
    }
    if (privacyOutcome.action === 'switch_local') {
      const local = findLocalOnlyTarget();
      if (!local) {
        store.addMessage(sessionId, {
          role: 'system',
          content: `${PRIVACY_NOTE_PREFIX} The resident turn was blocked from cloud egress and no genuinely local Ollama model is configured, so nothing was submitted.`,
        });
        return;
      }
      resolvedTarget = local;
      targetSnapshot = snapshotForResolvedTarget(local);
      if (!targetSnapshot) {
        throw new Error('The Privacy Firewall local fallback could not be frozen for the resident runner.');
      }
      applyTargetSwitch(local);
      targetHistory = stripImagesForTextOnlyTarget(history, local);
      store.addMessage(sessionId, {
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} Switched the resident turn to ${targetLabel(local)} before protected content could leave the machine.`,
      });
    } else {
      targetHistory = privacyOutcome.messages.slice(0, historyLength);
      frozenTextRefs = textRefs.map((reference, index) => ({
        ...reference,
        content: textContent(
          privacyOutcome.messages[historyLength + index]?.content ?? '',
        ),
      }));
      if (privacyOutcome.newlyRedacted.length > 0) {
        store.addMessage(sessionId, {
          role: 'system',
          content: `${PRIVACY_NOTE_PREFIX} Redacted ${privacyOutcome.newlyRedacted.length} sensitive item(s) from the resident turn snapshot before cloud submission: ${describeRedactions(privacyOutcome.newlyRedacted)}.`,
        });
      }
    }
  }

  if (resolvedTarget.kind === 'provider') {
    try {
      assertCostBudgetAllowsRequest(useCostControlStore.getState());
    } catch (error) {
      store.addMessage(sessionId, {
        role: 'system',
        content: errorMessage(error),
      });
      return;
    }
    if (settings.rateLimitWarningsEnabled) {
      for (const warning of evaluateRateLimit(
        resolvedTarget.providerId,
        settings.providerRateLimits[resolvedTarget.providerId],
      )) {
        const windowLabel = warning.window === 'minute' ? 'rolling minute' : 'rolling day';
        store.addMessage(sessionId, {
          role: 'system',
          content: `[Rate limit] The queued resident request is at ${Math.round(warning.percent * 100)}% of the configured ${warning.limit}-request ${windowLabel} cap (${warning.nextCount}/${warning.limit}).`,
        });
      }
    }
  }

  const frozenAttachments: FrozenAttachmentInput[] = [
    ...frozenTextRefs.map((reference) => ({
      path: reference.path,
      kind: reference.isDir ? 'directory' as const : 'file' as const,
      mediaType: 'text/plain; charset=utf-8',
      content: reference.content,
    })),
    ...images.map((image) => ({
      path: image.path,
      kind: 'image' as const,
      mediaType: image.dataUrl.slice(5, image.dataUrl.indexOf(';')) || 'application/octet-stream',
      content: image.dataUrl,
    })),
  ];
  const mode = usePermissionStore.getState().mode;
  const recipe = await buildDaemonDesktopRecipe({
    sessionId,
    turnId,
    userText,
    systemPrompt: composeSkillSystemPrompt(
      currentSystemPrompt(session?.personaId ?? null, attachedStacksForPrompt, docChatMode),
      skillInvocations,
    ),
    history: targetHistory,
    resolvedTarget,
    targetSnapshot,
    roots: useWorkspaceStore.getState().roots,
    permissionMode: mode,
    allowNetwork: settings.webToolsEnabled,
    memoryEnabled: settings.memoryEnabled,
    verifyEnabled: settings.verifyEnabled,
    verifyMaxRounds: settings.verifyMaxRounds,
    subagentsEnabled: settings.subagentsEnabled,
    effort: effortForTarget(resolvedTarget) ?? null,
    mcpServers: useMcpStore.getState().servers,
    attachedStackIds,
    attachedStackNames: attachedStacks.map((stack) => stack.name),
    attachments: frozenAttachments,
    workspaceMutationRequired: mutationRequired,
  });
  const queued = await submitDaemonDesktopTurn(turnId, recipe, origin);
  // Create the daemon job's process record here, with this turn as its parent.
  // The daemon's own per-tick reconcile then finds this record and only moves
  // its state, which is how the lineage edge survives crossing the process
  // boundary — the daemon has no way to know which turn queued a job.
  void reconcileProcess({
    kind: 'daemon_job',
    externalId: queued.job_id,
    state: 'admitted',
    parentKind: 'chat_turn',
    parentExternalId: turnId,
  });
  if (resolvedTarget.kind === 'provider') {
    recordRequest(resolvedTarget.providerId);
  }
  if (signal.aborted) {
    await daemonCancel(queued.run_id, 'Stopped before attach');
  }

  if (unresolved.length > 0) {
    store.addMessage(sessionId, {
      role: 'system',
      content: `${MENTION_NOTE_PREFIX} Couldn't read ${unresolved.map((path) => `@${path}`).join(', ')} — the resident snapshot contains only successfully resolved attachments.`,
    });
  }
  if (sourcesNotice) store.addMessage(sessionId, { role: 'system', content: sourcesNotice });
  const assistantIndex = sessionMessages(sessionId).length;
  store.addMessage(sessionId, { role: 'assistant', content: '⏳ Queued in the resident runner…' });
  const link: ActiveDaemonDesktopTurn = {
    sessionId,
    turnId,
    runId: queued.run_id,
    assistantIndex,
    lastSequence: 0,
    output: '',
    source: origin,
  };
  saveActiveDaemonTurn(link);
  const controller = turnControllers.get(sessionId);
  if (!controller) throw new Error('Desktop turn controller disappeared before daemon attach.');
  registerDurableController(queued.run_id, controller, sessionId);
  await attachDaemonTurnToChat(link, controller);
  maybeAutoPreviewNewestArtifact(sessionId, anchorIndex);
}

/** Reattaches chat placeholders to nonterminal daemon runs after an app or
 * WebView restart. The durable event sequence is replayed from the exact
 * last committed sequence stored with each link. */
export function recoverDaemonDesktopTurns(): void {
  for (const link of loadActiveDaemonTurns()) {
    if (turnControllers.has(link.sessionId)) continue;
    if (!useSessionStore.getState().sessions.some((session) => session.id === link.sessionId)) {
      removeActiveDaemonTurn(link.runId);
      continue;
    }
    const controller = new AbortController();
    turnControllers.set(link.sessionId, controller);
    useSessionStore.getState().markTurnRunning(link.sessionId, true);
    registerDurableController(link.runId, controller, link.sessionId);
    void attachDaemonTurnToChat(link, controller).finally(() => {
      turnControllers.delete(link.sessionId);
      cancellationDisposers.get(controller)?.forEach((dispose) => dispose());
      cancellationDisposers.delete(controller);
      externallyRequestedCancellations.delete(link.runId);
      useSessionStore.getState().markTurnRunning(link.sessionId, false);
    });
  }
}

/** When `artifactAutoPreview` (see `settingsStore.ts`) is on, opens the
 * newest previewable artifact (html/svg/mermaid fence, see `artifacts.ts`)
 * produced by the turn that just finished — filtered to `ref.messageIndex >=
 * anchorIndex` so an artifact already sitting earlier in the transcript from
 * a previous turn is never re-opened just because this turn happened to run.
 * Best-effort and silent: `extractArtifacts` is a pure re-scan of the
 * transcript, so "no previewable artifact this turn" is simply a no-op, not
 * an error. Called from `runTurnGuarded` right after `runAgentTurnBody`
 * returns — the turn-completion point the design doc calls out — so this
 * runs whether the turn ended in a plain answer, the tool-calling safety
 * cap, or a caught stream error; it does NOT run if `runAgentTurnBody`
 * itself throws, since there's no well-defined "finished assistant message"
 * in that case.
 *
 * `ArtifactPane` is a single shared surface across the main pane and the
 * split pane (see `artifactStore.ts`'s doc comment), and with the split pane
 * open, two turns can run fully concurrently in two different sessions (see
 * `runAgentTurn`'s per-session `turnControllers`). Without a guard, whichever
 * session's turn happens to finish LAST would silently steal the shared pane
 * away from whatever artifact the user is actually looking at for the OTHER
 * session — mid-read, mid-Save-As, whatever — with no indication anything
 * changed beyond the title. So this only ever opens into an empty pane, or
 * refreshes the pane for the SAME session it's already showing; a
 * background session's completed turn never reaches across and hijacks a
 * different session's open artifact. */
export function maybeAutoPreviewNewestArtifact(sessionId: string, anchorIndex: number): void {
  if (!useSettingsStore.getState().artifactAutoPreview) return;
  const active = useArtifactStore.getState().active;
  if (active && active.sessionId !== sessionId) return;
  const artifacts = extractArtifacts(sessionMessages(sessionId)).filter((a) => a.ref.messageIndex >= anchorIndex);
  if (artifacts.length === 0) return;
  useArtifactStore.getState().open(sessionId, artifacts[artifacts.length - 1].ref);
}

/** `runAgentTurn` minus the per-session turn registration — the checkpoint
 * lifecycle half of the wrapper.
 *
 * This is the in-process conversational loop, and the two checks below are what
 * keep it from being one on the desktop. It runs a user's turn inside the
 * webview, which is only defensible where nothing else on the machine can: a
 * profile with no Tauri bridge. Anywhere the bridge exists, a durable execution
 * authority is reachable and every accepted turn belongs to it.
 *
 * The environment is checked here and not only at the caller on purpose. Routing
 * already refuses, so this second check is dead code today — which is the point:
 * it is what a future caller that skips `daemonDesktopRoute` hits, so
 * reintroducing the bypass takes deleting a guard rather than forgetting one. */
async function runTurnGuarded(
  sessionId: string,
  userText: string,
  attachments: AttachmentRef[],
  signal: AbortSignal,
  turnId: string,
  skillInvocations: SkillInvocationSnapshot[],
  availableSkills: SlashSkill[] = [],
  ultracode = false,
  mutationRequired = false,
  route: ConversationRoute = 'browser',
): Promise<void> {
  if (route !== 'browser' || isTauri()) {
    throw new Error(
      'A conversational turn cannot be executed in the app process. The resident runner owns desktop execution.',
    );
  }
  // The index this turn's user message will land at — captured before
  // `addMessage` so it can anchor a later "Rewind conversation" back to the
  // state just before this turn.
  const anchorIndex = sessionMessages(sessionId).length;

  // Added as plain text first for instant feedback (resolving references
  // in the turn body does async file/image reads) — if there's at least one
  // image, it's promoted in place, right after, to a `ChatContentPart[]` so
  // the chat UI actually shows what was attached, not just what was typed.
  // No resume arm here, and that absence is load-bearing: a resumed turn is a
  // continuation of one the durable backend accepted, so it is watched
  // (`watchResumedDesktopTurn`) and never executed in this process. This
  // function is only reachable where no durable authority can exist at all.
  useSessionStore.getState().addMessage(sessionId, { role: 'user', content: userText });

  // Open a per-turn file checkpoint (see src-tauri/src/checkpoints.rs) so
  // every write_file/edit_file this turn makes can be reverted in one click.
  // Checkpoints are keyed by id — with the split pane, the other pane's turn
  // may hold its own concurrent checkpoint — so this turn's id is threaded
  // through to every file-mutating tool call and to checkpoint_end. The
  // session/anchor/label metadata ends up in the manifest for conversation
  // rewind and timeline labels; maxKeep is the retention cap, user-configurable
  // via AutomationPanel's "Keep last N checkpoints" setting (settingsStore).
  // Failure to open one (e.g. app-data dir unavailable) must never block the
  // turn itself — the turn just runs without a revert affordance.
  const checkpointId = await invoke<string>('checkpoint_begin', {
    sessionId,
    anchorIndex,
    label: userText.slice(0, CHECKPOINT_LABEL_MAX_CHARS),
    maxKeep: useSettingsStore.getState().checkpointRetention,
  }).catch(() => null);
  // Distinct from checkpointId (which can be null): scopes shell,
  // cancellation, permission prompts, and durable run events to this turn.
  const durable: DurableTurnContext = { recorder: null, failure: null, reflect: null, skills: null, toolFailures: [] };
  let thrown: unknown = null;
  try {
    await runAgentTurnBody(
      sessionId,
      userText,
      attachments,
      checkpointId,
      turnId,
      durable,
      signal,
      skillInvocations,
      availableSkills,
      ultracode,
      mutationRequired,
    );
    maybeAutoPreviewNewestArtifact(sessionId, anchorIndex);
  } catch (error) {
    thrown = error;
    throw error;
  } finally {
    if (checkpointId !== null) {
      const summary = await invoke<CheckpointNotice>('checkpoint_end', { id: checkpointId }).catch(() => null);
      if (summary) {
        durable.recorder?.recordCheckpoint(
          summary.id,
          summary.label ?? (userText.slice(0, CHECKPOINT_LABEL_MAX_CHARS) || 'Workspace checkpoint'),
        );
      }
      if (summary && summary.files.length > 0) {
        useSessionStore.getState().addMessage(sessionId, {
          role: 'system',
          content: formatCheckpointNotice({
            id: summary.id,
            files: summary.files,
            anchorIndex: summary.anchorIndex,
            label: summary.label,
            shellRan: summary.shellRan,
          }),
        });
      }
      // Invalidate the timeline's cache for this session — whether or not
      // this particular turn produced a checkpoint, retention pruning at the
      // next `checkpoint_begin` can also change what's on disk. Safe to fire
      // and forget: a panel that isn't open just gets a fresher cache.
      void useCheckpointStore.getState().refresh(sessionId);
    }
    if (durable.recorder) {
      if (signal.aborted && !externallyRequestedCancellations.delete(durable.recorder.runId)) {
        await requestRunCancellation(durable.recorder.runId, 'Stopped from chat').catch((error) =>
          console.error('Failed to record cancellation request', error),
        );
      }
      const cleanlyCompleted = !signal.aborted && thrown === null && durable.failure === null;
      const terminal = signal.aborted
        ? durable.recorder.cancel('Stopped by the user')
        : thrown !== null
          ? durable.recorder.fail(thrown)
          : durable.failure !== null
            ? durable.recorder.fail(new Error(durable.failure), true)
            : durable.recorder.complete('Desktop turn completed');
      await terminal.catch((error) => console.error('Failed to finalize durable run', error));

      // Learning runs strictly AFTER the run is durably complete.
      //
      // Effectiveness is finalized for EVERY terminal state — a failed or
      // cancelled turn that used a learned skill is exactly the turn its
      // history most needs — and the backend reads which versions the run
      // used from the run's own durable events, so nothing here names a hash.
      // Detection is the narrower half: only a run that actually completed
      // can be a candidate. Everything is best-effort — a turn that already
      // succeeded must never surface a learning failure as its own.
      await finalizeLearningForRun(sessionId, durable.recorder.runId, userText);
      if (cleanlyCompleted && durable.reflect !== null) {
        const scope: NativeSkillScope =
          primaryRoot(useWorkspaceStore.getState().roots) !== null ? 'workspace' : 'global';
        const candidate = await learnFromFinishedRun(
          durable.recorder.runId,
          userText,
          scope,
          durable.reflect,
          signal,
        );
        if (candidate) {
          useSessionStore.getState().addMessage(sessionId, {
            role: 'system',
            content: formatLearningNotice(candidateNotice(candidate)),
          });
        }
      }
    }
  }
}

/** The actual turn logic — split out so `runAgentTurn` can wrap it in the
 * per-turn checkpoint lifecycle with a single try/finally around every early
 * return this loop has. */
async function runAgentTurnBody(
  sessionId: string,
  userText: string,
  attachments: AttachmentRef[],
  checkpointId: string | null,
  turnId: string,
  durable: DurableTurnContext,
  signal?: AbortSignal,
  skillInvocations: SkillInvocationSnapshot[] = [],
  availableSkills: SlashSkill[] = [],
  ultracode = false,
  mutationRequired = false,
): Promise<void> {
  // Every transcript mutation this turn makes is pinned to the session the
  // turn was submitted from — the user may be running another turn in the
  // other pane (or reading a different session) meanwhile.
  const store = useSessionStore.getState();
  const addMessage = (msg: ChatMessage) => store.addMessage(sessionId, msg);
  const updateLastMessage = (patch: Partial<ChatMessage>) => store.updateLastMessage(sessionId, patch);
  const removeLastMessage = () => store.removeLastMessage(sessionId);
  const replaceMessages = (messages: ChatMessage[]) => store.replaceMessages(sessionId, messages);

  // Resolve any "@"-mentions and explicit attachments in the raw user text
  // *once*, up front, for this turn. Text references only ever expand the
  // *wire* payload (sessionStore keeps the unexpanded text the user typed),
  // but images are promoted into the stored message itself, right below.
  const { textRefs, images, unresolved } = await resolveReferences(userText, attachments);

  if (images.length > 0) {
    updateLastMessage({ content: toMessageContent(userText, images) });
  }
  // Re-read rather than reuse the object passed to `addMessage` above: the
  // `updateLastMessage` call just now (if it ran) replaced it with a new
  // object in the store, and this is the reference every later `===` match
  // against "this turn's user message" (for the wire-payload substitution
  // below, across every tool-calling round trip) needs to stay accurate.
  const storedMessages = sessionMessages(sessionId);
  const storedUserMessage = storedMessages[storedMessages.length - 1];

  // Surface any "@"-mentions that failed to resolve — the model only sees
  // them as plain text, and without this the user never learns why. Added
  // only after `storedUserMessage` is captured above, so the notice can't
  // become the "last message" the image promotion patches.
  if (unresolved.length > 0) {
    addMessage({
      role: 'system',
      content: `${MENTION_NOTE_PREFIX} Couldn't read ${unresolved.map((p) => `@${p}`).join(', ')} — check the path${unresolved.length > 1 ? 's exist and are' : ' exists and is'} readable. The mention was sent as plain text only.`,
    });
  }

  const composedText = composeReferencedText(userText, textRefs);
  let wireContent = textRefs.length > 0 ? toMessageContent(composedText, images) : null;
  const requireVision = images.length > 0;

  const settings = useSettingsStore.getState();
  const privacyWorkspaceId = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? 'global';
  const privacyWireCache: PrivacyWireCache = new Map();
  const surfacedRateLimitWarnings = new Set<string>();

  if (!activeTargetSatisfiesVision(requireVision)) {
    if (settings.autoVisionSwitchEnabled) {
      const candidate = findVisionCandidate();
      if (candidate) {
        applyTargetSwitch(candidate);
        addMessage({ role: 'system', content: `${SWITCH_NOTE_PREFIX} Switched to ${targetLabel(candidate)} — it can see images.` });
      } else {
        addMessage({
          role: 'system',
          content: `${SWITCH_NOTE_PREFIX} No vision-capable model is configured — the attached image may not be understood.`,
        });
      }
    } else {
      addMessage({
        role: 'system',
        content: `${SWITCH_NOTE_PREFIX} The active model can't see images and auto-switch is off — the attached image may not be understood.`,
      });
    }
  }

  // The ordered attempt sequence for this turn: the (possibly just-switched)
  // active target first, then — only when auto-failover is on and that
  // target is a cloud provider — the rest of `buildFailoverChain`. Computed
  // once per turn and only advanced (never rebuilt) on a pre-first-token
  // failure, so a target that succeeds stays in use for every subsequent
  // tool round trip within this same turn.
  //
  // K9 dispatch policy runs here: after the vision auto-switch (so a policy
  // sees the target the user would actually have run) and before the Privacy
  // Firewall gate below (so the firewall still owns the final say over a
  // routed target, including its own switch-to-local).
  const routed = await routeTarget({
    taskClass: 'chat',
    requiresVision: requireVision,
    requiresTools: true,
  });
  let primaryTarget = routed.target;
  if (routed.decision.changedFromActive) {
    applyTargetSwitch(primaryTarget);
    addMessage({ role: 'system', content: `${ROUTING_NOTE_PREFIX} ${routed.decision.reason}` });
  }

  // Distinct from the block above: that one is about a NEW image attached
  // THIS turn; this one is an OLDER image already sitting in history from a
  // previous turn, now that the resolved target can't see it (e.g. the user
  // switched to a text-only model since). Deliberately not auto-switched —
  // unlike the new-attachment case, silently jumping back to a vision model
  // on every later turn would undo the user's own model choice — so this
  // just surfaces a notice and leaves the decision to them.
  // `stripImagesForTextOnlyTarget` (used below, per-target) does the actual
  // stripping so the request itself doesn't fail either way.
  if (!requireVision && !resolvedTargetSupportsVision(primaryTarget) && storedMessages.some((m) => Array.isArray(m.content) && m.content.some((p) => p.type === 'image_url'))) {
    addMessage({
      role: 'system',
      content: `${SWITCH_NOTE_PREFIX} This conversation has an earlier image that ${targetLabel(primaryTarget)} can't see — this reply won't see it either. Switch to a vision-capable model if you need it referenced.`,
    });
  }

  // Privacy Firewall (ROADMAP.md Phase 5): a visible data boundary before
  // this turn's content — `composedText`, i.e. the typed message plus every
  // "@"-mention/attachment it expanded into above, so a secret hiding in a
  // referenced FILE is caught exactly like one typed directly — leaves the
  // machine for a CLOUD model. Local llama.cpp and Ollama targets never
  // leave the machine, so the gate is skipped entirely for those. Run after
  // the vision-switch checks above so their notices describe the model the
  // user actually picked, not one this gate might still redirect away from.
  //
  // This first check exists so a local-only decision can select the durable
  // run's correct target before recording begins. It is not the only check:
  // `privacyGateWireForTarget` below gates the complete wire payload before
  // every provider request, including later tool/MCP/subagent results,
  // compaction/risk side calls, retries, and failover. Connector writes,
  // remote runners, and paired-device egress still use their own destination
  // boundaries rather than this cloud-model path.
  if (primaryTarget.kind === 'provider') {
    const gate = await usePrivacyFirewallStore
      .getState()
      .gateOutbound(composedText, 'cloud_model', privacyWorkspaceId);

    if (gate.action === 'cancelled') {
      addMessage({
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} This turn was blocked from being sent to ${targetLabel(primaryTarget)} and was cancelled.`,
      });
      return;
    }

    if (gate.action === 'switch_local') {
      const local = findLocalOnlyTarget();
      if (!local) {
        addMessage({
          role: 'system',
          content: `${PRIVACY_NOTE_PREFIX} This turn was blocked from being sent to ${targetLabel(primaryTarget)}, and no local-only model is configured to switch to — the turn was cancelled. Configure an Ollama model to enable the local-only fallback.`,
        });
        return;
      }
      addMessage({
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} Switched to ${targetLabel(local)} — this turn's content was blocked from leaving the machine.`,
      });
      primaryTarget = local;
      applyTargetSwitch(local);
    } else {
      // Seed the turn-scoped wire cache with the decision the user just made
      // (or the automatic allow/redact result). Every provider round-trip
      // below re-gates the complete outbound payload so tool results, RAG,
      // rules, and compaction prompts cannot bypass the firewall; caching
      // this first decision prevents the current user message from opening
      // the same approval twice.
      privacyWireCache.set(composedText, { content: gate.content });
      if (gate.content !== composedText) {
        const redacted = gate.report.findings.filter(
          (finding) => finding.action !== 'allow' && !finding.exempted,
        );
        addMessage({
          role: 'system',
          content: `${PRIVACY_NOTE_PREFIX} Redacted ${redacted.length} sensitive item(s) before sending to ${targetLabel(primaryTarget)}: ${describeRedactions(redacted)}.`,
        });
        wireContent = toMessageContent(gate.content, images);
      }
    }
  }

  const initialProviderTarget =
    primaryTarget.kind === 'provider' ? primaryTarget : null;
  // A matched K9 policy supplies this turn's attempt order, replacing the
  // fixed provider chain — "failover follows a fixed sequence" was the other
  // half of what K9 owed. Ignored when the Privacy Firewall redirected this
  // turn (it reassigns `primaryTarget` above, and its decision outranks any
  // policy), and still gated on the same `autoFailoverEnabled` toggle, so
  // turning failover off means one attempt whether a policy matched or not.
  const policySequence =
    primaryTarget === routed.target && routed.sequence.length > 0 ? routed.sequence : null;
  let sequence: ResolvedTarget[];
  if (!settings.autoFailoverEnabled) {
    sequence = [primaryTarget];
  } else if (policySequence) {
    sequence = policySequence;
  } else if (initialProviderTarget) {
    sequence = [
      initialProviderTarget,
      ...buildFailoverChain(requireVision).filter(
        (candidate) =>
          !(
            candidate.kind === 'provider'
            && candidate.providerId === initialProviderTarget.providerId
            && candidate.model === initialProviderTarget.model
          ),
      ),
    ];
  } else {
    sequence = [primaryTarget];
  }
  let sequenceIndex = 0;
  let target = sequence[0];

  // Per-model, so it must track the target: re-resolved below whenever a
  // failover switch changes `target` mid-turn.
  let effort = effortForTarget(primaryTarget);

  // Read once per turn — not re-derived on every
  // tool-calling round trip, so a mode switch mid-turn (possible via the
  // split pane's shared global mode, or the user clicking Approve on a plan
  // card mid-turn) never changes what's offered partway through this turn's
  // own tool-calling loop. See `toolsForMode`'s doc comment for why
  // `present_plan` is gated on this snapshot.
  const mode = usePermissionStore.getState().mode;
  // Re-check the frozen permission mode inside the actual loop. The outer
  // preflight already excludes Plan Mode, but this keeps a mode switch during
  // early async setup from turning a read-only planning turn into an enforced
  // write contract.
  const enforceMutation = mutationRequired && mode !== 'plan';

  const startDurableRecorder = async (
    resolvedTarget: ResolvedTarget,
    runId: string,
  ): Promise<DurableRunRecorder | null> => {
    const targetSnapshot = snapshotForResolvedTarget(resolvedTarget);
    if (!targetSnapshot) return null;
    return beginDurableRun({
      runId,
      kind: 'interactive',
      task: userText,
      instructions: `Session ${sessionId}`,
      target: targetSnapshot,
      roots: useWorkspaceStore.getState().roots,
      permissionMode: mode,
      allowNetwork: settings.webToolsEnabled,
      allowExternalMutations: mode !== 'plan',
    });
  };
  // Durable recording is additive during the engine migration. A profile
  // opened by an older host still runs normally; the protocol-version probe
  // inside `beginDurableRun` returns null in that case.
  durable.recorder = await startDurableRecorder(primaryTarget, turnId).catch((error) => {
    console.error('Failed to start durable run', error);
    return null;
  });
  // Recorded here rather than at the `routeTarget` call above, because the run
  // it belongs to did not exist yet up there. The *what* — the frozen
  // `ModelTargetSnapshot` — was written by `startDurableRecorder` a line ago;
  // this is the *why*, which K9's entry named as the half missing from the
  // ledger. Recorded even when no policy matched: "nothing routed this" is the
  // answer for a fresh profile, and an absent event cannot be told apart from
  // one that was never written.
  durable.recorder?.recordRoutingDecision(routed.decision);
  // Now that the run row exists, point this turn's process row at it, so the
  // per-process resource ledger can charge this run's measured usage (CPU, RSS,
  // egress bytes) to the turn that caused it. It cannot be done at admission
  // time: `runAgentTurn` mints the row before any run exists, and
  // `agent_processes.run_id` is a foreign key into `runs` — see
  // `linkProcessRun`.
  //
  // Exactly one link, for this first run only. `switchTurnToLocalForPrivacy`
  // below starts a SECOND durable run and deliberately does not re-link: moving
  // the row would leave this run's already-measured bytes claimed by nobody,
  // which the ledger buckets as unattributed just like a double claim. The
  // daemon route never reaches here at all, and must not — the `daemon_job` row
  // is the single claimant of the daemon's run id.
  if (durable.recorder) {
    const turnProcessId = chatTurnProcesses.get(turnId);
    if (turnProcessId) await linkProcessRun(turnProcessId, durable.recorder.runId);
  }

  const switchTurnToLocalForPrivacy = async (
    blockedTarget: ResolvedTarget,
  ): Promise<ResolvedTarget | null> => {
    const local = findLocalOnlyTarget();
    if (!local) {
      addMessage({
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} Outbound content was blocked from ${targetLabel(blockedTarget)}, and no local-only model is configured — the turn was cancelled.`,
      });
      await durable.recorder
        ?.cancel('Privacy Firewall blocked cloud egress; no local model was available.')
        .catch((error) => console.error('Failed to cancel privacy-blocked durable run', error));
      return null;
    }

    await durable.recorder
      ?.cancel('Privacy Firewall switched the remaining turn to a local model.')
      .catch((error) => console.error('Failed to close cloud durable run after privacy switch', error));
    primaryTarget = local;
    target = local;
    effort = effortForTarget(local);
    sequence = [local];
    sequenceIndex = 0;
    applyTargetSwitch(local);
    addMessage({
      role: 'system',
      content: `${PRIVACY_NOTE_PREFIX} Switched to ${targetLabel(local)} before protected outbound content could leave the machine.`,
    });
    durable.recorder = await startDurableRecorder(local, `${turnId}-privacy-local`).catch((error) => {
      console.error('Failed to start local durable run after privacy switch', error);
      return null;
    });
    if (durable.recorder) {
      const controller = turnControllers.get(sessionId);
      if (controller) registerDurableController(durable.recorder.runId, controller, sessionId);
    }
    return local;
  };

  const surfaceRateLimitWarnings = (candidate: ResolvedTarget): void => {
    if (
      candidate.kind !== 'provider'
      || !settings.rateLimitWarningsEnabled
    ) {
      return;
    }
    const warnings = evaluateRateLimit(
      candidate.providerId,
      settings.providerRateLimits[candidate.providerId],
    );
    for (const warning of warnings) {
      const key = `${warning.providerId}:${warning.window}:${warning.severity}`;
      if (surfacedRateLimitWarnings.has(key)) continue;
      surfacedRateLimitWarnings.add(key);
      const windowLabel = warning.window === 'minute' ? 'rolling minute' : 'rolling day';
      const stateLabel =
        warning.severity === 'exceeded'
          ? 'would exceed'
          : `is at ${Math.round(warning.percent * 100)}% of`;
      addMessage({
        role: 'system',
        content: `[Rate limit] The next ${warning.providerId} request ${stateLabel} your configured ${warning.limit}-request ${windowLabel} cap (${warning.nextCount}/${warning.limit}). This is a local warning based on your own cap, not a provider guarantee.`,
      });
    }
  };

  /**
   * Gates every string in an imminent provider request, not just the typed
   * prompt. That includes system/rules context, retrieved sources, tool/MCP
   * results, subagent reports, compaction prompts, retries, and failovers.
   * Raw transcript state stays untouched; only this returned wire copy may
   * contain redactions.
   */
  const privacyGateWireForTarget = async (
    candidate: ResolvedTarget,
    messages: ChatMessage[],
  ): Promise<{ target: ResolvedTarget; messages: ChatMessage[] } | null> => {
    if (candidate.kind !== 'provider') {
      return { target: candidate, messages };
    }
    const outcome = await gatePrivacyWireMessages(
      messages,
      (content) =>
        usePrivacyFirewallStore
          .getState()
          .gateOutbound(content, 'cloud_model', privacyWorkspaceId),
      privacyWireCache,
    );
    if (outcome.action === 'cancelled') {
      addMessage({
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} Protected outbound content was blocked from ${targetLabel(candidate)} and the turn was cancelled before the request was sent.`,
      });
      await durable.recorder
        ?.cancel('Privacy Firewall cancelled protected cloud egress.')
        .catch((error) => console.error('Failed to cancel privacy-blocked durable run', error));
      return null;
    }
    if (outcome.action === 'switch_local') {
      const local = await switchTurnToLocalForPrivacy(candidate);
      return local ? { target: local, messages } : null;
    }
    if (outcome.newlyRedacted.length > 0) {
      addMessage({
        role: 'system',
        content: `${PRIVACY_NOTE_PREFIX} Redacted ${outcome.newlyRedacted.length} sensitive item(s) from protected context before sending to ${targetLabel(candidate)}: ${describeRedactions(outcome.newlyRedacted)}.`,
      });
    }
    surfaceRateLimitWarnings(candidate);
    return { target: candidate, messages: outcome.messages };
  };

  // Pick up any external edits to MONKEY.md (and, once slice 3 lands,
  // newly remembered facts) before building the system prompt below — two
  // local-file reads per turn is negligible and needs no file watcher.
  await useRulesStore.getState().refresh();

  // Computed once per turn (not re-derived on every tool-calling round trip)
  // so a server that connects/disconnects mid-turn doesn't change what's on
  // offer between one model round trip and the next within the same turn —
  // mirrors how `sequence`/`target` above are also fixed for the turn.
  // `mcpRegistry` is THIS turn's own resolution table (see `mcpTools.ts`'s
  // doc comment) — with the split pane, another turn's concurrent
  // `mcpToolDefs()` call must never be able to invalidate or repoint a name
  // this turn's model was already offered, so it's threaded through to
  // `executeToolCall` explicitly rather than read back out of shared state.
  const { defs: mcpDefs, registry: mcpRegistry } = mcpToolDefs();
  const { defs: extensionDefs, registry: extensionRegistry } = await executableExtensionToolDefs();

  // This session's attached knowledge stacks (see `ChatSession.attachedStackIds`,
  // `StackPicker.tsx`), resolved against the current stack registry once per
  // turn — same "computed once, not re-derived every round trip" stance as
  // `mode`/`mcpDefs` above. Empty for the overwhelming majority of turns (no
  // stacks attached), in which case `buildTools` returns the base list
  // unchanged and the system prompt gets no stacks guidance line.
  const attachedStackIds = useSessionStore.getState().sessions.find((s) => s.id === sessionId)?.attachedStackIds ?? [];
  const attachedStacks =
    attachedStackIds.length > 0
      ? useStackStore.getState().stacks.filter((stack) => attachedStackIds.includes(stack.id))
      : [];
  const attachedStackNames = attachedStacks.map((stack) => stack.name);
  const attachedStacksForPrompt = attachedStackPromptInfo(attachedStacks);

  // This session's doc-chat toggle (see `ChatSession.docChatMode`,
  // `StackPicker.tsx`) — read once per turn, same stance as `attachedStackIds`
  // just above. Used both for the auto-retrieval block right below and for
  // every iteration's system prompt (see `currentSystemPrompt`'s call inside
  // the loop), so a toggle flipped mid-turn never changes behavior partway
  // through this turn's own tool-calling loop.
  const docChatMode = useSessionStore.getState().sessions.find((s) => s.id === sessionId)?.docChatMode ?? false;

  // Doc-chat mode (RAG design doc slice 3): auto-retrieve the top-k passages
  // for this turn's own message BEFORE the first model call below, so the
  // model never has to remember to call `search_docs` itself. Gated on at
  // least one attached stack — with none, there's nothing to search, and
  // `stacks_query` would just error. The notice is appended via `addMessage`
  // exactly like every other synthetic notice in this module (see
  // `SOURCES_NOTE_PREFIX`'s doc comment for why this alone is enough to make
  // it show up in every subsequent iteration's wire payload AND count toward
  // `contextTrimmer.ts`'s token estimate). Retrieval failure — a stack mid-
  // reindex, the embed server down, a corrupt index — must never block the
  // turn, so it's swallowed silently: the model just proceeds without
  // sources for this turn, same as an unattached session always has.
  if (docChatMode && attachedStackIds.length > 0 && !signal?.aborted) {
    try {
      const hits = await invoke<StackQueryResult[]>('stacks_query', { stackIds: attachedStackIds, query: userText });
      if (hits.length > 0) {
        addMessage({
          role: 'system',
          content: formatSourcesNotice({
            results: hits.map((hit) => ({ path: hit.source_path, stack: hit.stack_name, score: hit.score, snippet: hit.text })),
          }),
        });
      }
    } catch {
      // See doc comment above — retrieval failure must never block the turn.
    }
  }

  // Every command already invoked this turn — explicit `/command`
  // invocations are already known before the loop starts; a model-invoked
  // `skill` tool call (see `turnEngine.ts`'s `SkillToolContext`) mutates this
  // SAME `Set` in place as the turn progresses, so both the catalog
  // (`composeSkillCatalog`, below) and the `allowed-tools` restriction
  // (`toolsOfferedThisIteration`, inside the loop) see later model-invoked
  // skills too, not just the ones known up front.
  const invokedSkillCommands = new Set(skillInvocations.map((invocation) => invocation.skill.command));
  const skillToolContext: SkillToolContext = {
    availableSkills,
    invokedCommands: invokedSkillCommands,
    maxSkillsPerTurn: MAX_SKILLS_PER_TURN,
    runId: durable.recorder?.runId,
    onInvoked: (skill) => recordSkillInvocation(durable, skill),
  };
  durable.skills = skillToolContext;
  // Explicit `/command` invocations are already frozen into the system prompt
  // by the time the loop starts, so they are recorded here rather than through
  // `onInvoked` — the event has to name every version this run used, not only
  // the ones the model picked itself.
  for (const invocation of skillInvocations) recordSkillInvocation(durable, invocation.skill);
  const skillToolEnabled =
    settings.skillAutoInvokeEnabled && availableSkills.some((candidate) => !invokedSkillCommands.has(candidate.command));
  const readSkillResourceToolEnabled = availableSkills.some((candidate) => (candidate.resourceFiles?.length ?? 0) > 0);
  // `GENERATE_IMAGE_TOOL` is appended here (desktop chat's composition chain
  // only) rather than living in the base `TOOLS` array — see its doc comment
  // in tools.ts for why (webview-rasterized wire shape; monkey-cli can't
  // offer it). The result lives in private app storage, not the workspace,
  // so it has no edit-permission or selected-folder dependency.
  const hasWorkspace = primaryRoot(useWorkspaceStore.getState().roots) !== null;
  const baseToolsForTurn: ToolDef[] = toolsForWorkspace(
    toolsForSettings(
      toolsForMode([...buildTools(attachedStackNames), GENERATE_IMAGE_TOOL, ...mcpDefs, ...extensionDefs], mode),
      settings.memoryEnabled,
      settings.webToolsEnabled,
      // Ultracode force-offers the `task` tool even when the subagents toggle
      // is off: selecting Ultracode is itself the user's explicit opt-in to
      // multi-agent orchestration for this turn (see ULTRACODE_SYSTEM_SECTION).
      settings.subagentsEnabled || ultracode,
      skillToolEnabled,
      readSkillResourceToolEnabled,
      // The learning tool is a capability, so an unknown backend mode means
      // "not offered" — see `cachedLearningMode`. It also needs a durable run:
      // without one there is no evidence chain for a proposal to append to.
      cachedLearningMode() !== null && cachedLearningMode() !== 'off' && durable.recorder !== null,
    ),
    hasWorkspace,
  );

  const sendForSummary = async (dropped: ChatMessage[]): Promise<string> => {
    const summaryMessages: ChatMessage[] = [
      {
        role: 'system',
        content:
          'Summarize the following earlier conversation concisely for another AI assistant to continue from. Preserve key facts, decisions, file paths, and code context. Reply with only the summary text.',
      },
      { role: 'user', content: renderForSummary(dropped) },
    ];
    const prepared = await privacyGateWireForTarget(target, summaryMessages);
    if (!prepared) {
      throw new Error('Privacy Firewall cancelled context summarization.');
    }
    const result = await attemptStream(
      prepared.target,
      prepared.messages,
      [],
      signal,
      effort,
      sessionId,
      undefined,
      true,
      undefined,
      durable.recorder?.runId,
      true,
      { preGated: true },
    );
    if (result.streamError) throw new Error(result.streamError);
    return result.content.trim() || '(summary unavailable)';
  };

  // Advisory risk annotations (Phase 2 of the Plan/Act + risk-adaptive
  // permissions design — docs/roadmap/p2-plan-act-safety.md): built once per
  // turn (`cache` must persist across every tool-calling round trip below,
  // exactly like `mutatedFiles` just below it), passed unchanged into every
  // `executeToolCall` call this turn. `classify` mirrors `sendForSummary`
  // above almost exactly — the same `attemptStream`-against-`target` shape,
  // just wrapping `riskJudge.ts`'s `classifyToolCall` instead of a plain
  // summarization prompt (see that module's doc comment for why it takes this
  // callback as a parameter instead of importing `attemptStream` itself).
  const workspaceRootPath = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? '';
  const riskAnnotation: RiskAnnotationContext = {
    // "smart" mode (Phase 3) needs a classification for every mutating call
    // to decide whether it can auto-approve — so classification runs
    // whenever the user opted into the advisory badges OR is in "smart"
    // mode, even if they never flipped the AutomationPanel toggle on.
    enabled: settings.riskAnnotationsEnabled || mode === 'smart',
    cache: new Map<string, RiskClassification | null>(),
    classify: (toolName, toolArgs) =>
      classifyToolCall(
        toolName,
        toolArgs,
        workspaceRootPath,
        async (judgeMessages, judgeSignal) => {
          const prepared = await privacyGateWireForTarget(target, judgeMessages);
          if (!prepared) {
            throw new Error('Privacy Firewall cancelled risk classification.');
          }
          return attemptStream(
              prepared.target,
              prepared.messages,
              [],
              judgeSignal,
              effort,
              sessionId,
              undefined,
              true,
              undefined,
              durable.recorder?.runId,
              // Judge calls are side-channel work, not the turn's own output —
              // keep them out of the "✳ … N tokens" status line.
              false,
              { preGated: true },
            );
        },
        signal
      ),
  };

  // The bounded learning reflection pass shares `sendForSummary`/`classify`'s
  // exact transport — one non-streaming, privacy-gated call against this
  // turn's own target — and is stashed on the durable context because it can
  // only run once `runAgentTurn`'s `finally` has completed the run.
  durable.reflect = async (reflectionMessages, reflectionTools, reflectionSignal) => {
    const prepared = await privacyGateWireForTarget(target, reflectionMessages);
    if (!prepared) {
      throw new Error('Privacy Firewall cancelled the learning reflection.');
    }
    const result = await attemptStream(
      prepared.target,
      prepared.messages,
      reflectionTools,
      reflectionSignal,
      effort,
      sessionId,
      undefined,
      true,
      undefined,
      durable.recorder?.runId,
      // Side-channel work, like the risk judge — kept out of the turn's own
      // token status line.
      false,
      { preGated: true },
    );
    return { content: result.content, toolCalls: result.toolCalls, streamError: result.streamError };
  };

  // Absolute paths this turn's `write_file`/`edit_file` calls have
  // successfully mutated so far, across every tool-calling round trip below
  // — read by `runVerificationPhase` at the loop's natural exit to decide
  // whether there's anything worth verifying. Populated in the tool-call
  // loop right below via `isSuccessfulMutationResult`/`toolCallPathArg`.
  const mutatedFiles = new Set<string>();
  // Unlike `mutatedFiles`, this is intentionally never cleared after a
  // verification failure: the contract asks whether this turn ever changed a
  // real file, while the Set is reused to decide whether a new verification
  // pass is needed for only the latest edit round.
  let mutationSucceeded = false;
  let mutationCorrectiveRetryUsed = false;
  let mutationCorrectionPending = false;
  // Failed mutation calls remain unresolved until a later successful
  // write/edit targets the same path. A per-path map lets the model recover
  // from an old_string miss without allowing a success on some other file to
  // conceal the failure.
  const unresolvedMutationFailures = new Map<string, string>();

  // How many verification feed-back rounds have been consumed so far this
  // turn — compared against `settings.verifyMaxRounds` (default 1, clamp
  // 0-3) at the loop's natural exit below. A round is "consumed" the moment
  // a failure notice's fix instruction is appended and the loop is sent
  // around again, regardless of whether that round's edits actually fix
  // anything — this is what makes `verifyMaxRounds` a hard bound rather than
  // a "keep trying until it passes" loop.
  let verifyRound = 0;

  /**
   * The turn's safe point: freeze first if a suspend is latched, then park.
   *
   * This *is* the tool boundary K13's acceptance names. The previous round's
   * tool calls and their permission prompts have all resolved by here, which is
   * what makes the image coherent — and why it records no pending approvals
   * rather than recording an empty list as a shortcut.
   *
   * The image is written before the wait, not after it. A process parked in
   * memory is exactly the state a quit or a crash destroys, so an image written
   * on the way out would be written at the one moment it is too late.
   * Best-effort by construction: a freeze that fails leaves a turn that pauses
   * and resumes in memory, which is what happened before it existed.
   */
  async function parkHere(): Promise<void> {
    if (!signal) return;
    const processId = chatTurnProcesses.get(turnId) ?? null;
    if (isPauseRequested(turnId) && checkpointId !== null && processId !== null) {
      await invoke('checkpoint_freeze_live', {
        id: checkpointId,
        resume: {
          processId,
          frozenAtMs: Date.now(),
          model: describeUsageTarget(primaryTarget),
          runtimeId: primaryTarget.kind,
          workspace: primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null,
          pendingApprovals: [],
        },
      }).catch(() => undefined);
    }
    await honourPause(turnId, processId, signal);
  }

  // UserPromptSubmit hooks fire once per turn, before the first round trip;
  // their stdout joins the system prompt's sections below for EVERY iteration
  // of this turn (a hook that fails or times out contributes nothing — see
  // `userHooks.ts`'s failure posture).
  const userPromptHookContext = await collectUserPromptSubmitContext(sessionId);

  for (let iteration = 0; iteration < MAX_ITERATIONS; iteration++) {
    if (signal) await parkHere();
    // Stop button fired while a tool call was executing (between model
    // round trips, where there's no stream to abort) — don't start another.
    if (signal?.aborted) return;

    if (settings.contextTrimEnabled) {
      const current = sessionMessages(sessionId);
      if (shouldTrim(current, useUsageStore.getState().contextLimit, settings.contextTrimThreshold)) {
        const result = await applyContextCompaction(current, {
          strategy: settings.contextTrimStrategy,
          contextLimit: useUsageStore.getState().contextLimit,
          thresholdPercent: settings.contextTrimThreshold,
          sendForSummary,
        });
        if (result.changed) replaceMessages(result.messages);
      }
    }

    // Snapshot the history to send *before* appending this turn's in-progress
    // assistant placeholder, so the placeholder's (currently empty) content
    // never gets sent back to the model as part of its own history.
    const history: ChatMessage[] = sessionMessages(sessionId);

    // The system prompt (identity, workspace roots, OS, tool guidance,
    // MONKEY.md rules/facts, and the active persona — see systemPrompt.ts) is
    // injected at the head of the OUTGOING payload only, never stored in the
    // session transcript. Rebuilt every iteration (not just once before the
    // loop) so a `remember` call earlier in *this* turn — which refreshes
    // rulesStore right below — actually shows up in the system prompt sent
    // for the next round trip, instead of only from the next user turn
    // onward; the session's `personaId` is re-read fresh here too, for the
    // same reason (a persona switch or deletion mid-turn takes effect on the
    // very next round trip, and a dangling id just resolves to no persona —
    // see `resolvePersona`).
    const personaId = useSessionStore.getState().sessions.find((s) => s.id === sessionId)?.personaId ?? null;
    // Recomputed every iteration (not just once before the loop, same
    // "rebuilt every round trip" stance as `systemMessage` below): a
    // model-invoked `skill` tool call in an EARLIER iteration of this same
    // turn may have added to `invokedSkillCommands`, which can newly trigger
    // (or, if that skill declared no `allowedTools`, newly LIFT) an
    // `allowed-tools` restriction for this next round trip — see
    // `allowedToolsRestriction`'s doc comment.
    const toolsForTurn = applyAllowedToolsRestriction(
      baseToolsForTurn,
      allowedToolsRestriction(invokedSkillCommands, availableSkills),
    );
    const systemMessage: ChatMessage = {
      role: 'system',
      content: [
        composeSkillSystemPrompt(
          currentSystemPrompt(personaId, attachedStacksForPrompt, docChatMode),
          skillInvocations,
        ),
        ...(ultracode ? [ULTRACODE_SYSTEM_SECTION] : []),
        ...(settings.skillAutoInvokeEnabled ? [composeSkillCatalog(availableSkills, invokedSkillCommands)] : []),
        // Saved workflows are only actionable when WORKFLOW_TOOL is offered,
        // so the catalog rides the same `subagentsEnabled` gate.
        ...(settings.subagentsEnabled
          ? [composeSavedWorkflowCatalog(selectSavedWorkflowList(useSavedWorkflowStore.getState()))]
          : []),
        // Custom agents are `profile` values of TASK_TOOL/WORKFLOW_TOOL, so
        // the catalog rides the same gate that offers those tools — which
        // includes an Ultracode turn's force-offer (see `toolsForSettings`'s
        // call above).
        ...(settings.subagentsEnabled || ultracode
          ? [composeCustomAgentCatalog(selectCustomAgentList(useCustomAgentStore.getState()))]
          : []),
        ...(userPromptHookContext ? [userPromptHookContext] : []),
      ]
        .filter(Boolean)
        .join('\n\n'),
    };

    // Build the wire payload for this request: the system prompt first, then
    // `history` — identical to the stored transcript unless this turn's user
    // message had text references to expand, in which case that one message
    // (matched by reference) is swapped for its expanded content — `history`
    // itself (and what's stored/rendered) is left untouched. No substitution
    // is needed when there were no text references (`wireContent === null`):
    // `storedUserMessage` already carries any images directly.
    const fullWireHistory: ChatMessage[] = [
      systemMessage,
      ...(wireContent !== null
        ? history.map((message) => (message === storedUserMessage ? { ...message, content: wireContent } : message))
        : history),
      ...(mutationCorrectionPending
        ? [{ role: 'system' as const, content: WORKSPACE_MUTATION_CORRECTION }]
        : []),
    ]
      // `/btw` side-question exchanges are display-only: stored in the
      // transcript but never part of the conversation a model sees.
      .filter((message) => !isBtwNotice(message))
      .map(hardenSourcesNoticeForModel);
    // Strips any image content left over from earlier turns when `target`
    // can't see images — see `stripImagesForTextOnlyTarget`'s doc comment.
    const wireHistoryFor = (t: ResolvedTarget): ChatMessage[] => stripImagesForTextOnlyTarget(fullWireHistory, t);

    const targetBeforePrivacyGate = target;
    const preparedWire = await privacyGateWireForTarget(
      targetBeforePrivacyGate,
      wireHistoryFor(targetBeforePrivacyGate),
    );
    if (!preparedWire) return;
    target = preparedWire.target;
    const outboundWireHistory =
      target === targetBeforePrivacyGate
        ? preparedWire.messages
        : wireHistoryFor(target);

    const assistantPlaceholder: ChatMessage = { role: 'assistant', content: '' };
    addMessage(assistantPlaceholder);

    let attempt = await attemptStream(
      target,
      outboundWireHistory,
      toolsForTurn,
      signal,
      effort,
      sessionId,
      (content) => updateLastMessage({ content }),
      true,
      undefined,
      durable.recorder?.runId,
      true,
      { preGated: true },
    );

    // Some cloud routes (notably free-tier OpenRouter models) reject a
    // request outright just because tools were offered, even when the model
    // never intended to call one this turn. Retry the SAME target once with
    // no tools instead of treating it as a dead target — switching providers
    // or surfacing a raw error would be wrong when the model is otherwise
    // fine for a plain-text reply. Only tool_calls can be lost this way
    // (`toolsForTurn` empty on retry means the model literally cannot emit
    // one), so the outer round-trip loop just sees a normal plain answer.
    if (
      attempt.streamError !== null
      && !attempt.contentStarted
      && toolsForTurn.length > 0
      && canRetryWithoutTools(enforceMutation)
      && TOOL_UNSUPPORTED_ERROR_PATTERN.test(attempt.streamError)
    ) {
      durable.recorder?.recordStatus(
        `status-${turnId}-${iteration}-tools`,
        `Target rejected tool calling; retrying without tools: ${attempt.streamError}`,
      );
      if (attempt.usage) {
        durable.recorder?.recordUsage(attempt.usage.promptTokens, attempt.usage.completionTokens);
      }
      removeLastMessage();
      addMessage({
        role: 'system',
        content: `${SWITCH_NOTE_PREFIX} ${targetLabel(target)} doesn't support tool calling — retrying this turn without tools.`,
      });
      surfaceRateLimitWarnings(target);
      addMessage({ role: 'assistant', content: '' });
      attempt = await attemptStream(
        target,
        outboundWireHistory,
        [],
        signal,
        effort,
        sessionId,
        (content) => updateLastMessage({ content }),
        true,
        undefined,
        durable.recorder?.runId,
        true,
        { preGated: true },
      );
    }

    // Failover: only ever retry a *different* target when nothing streamed
    // back yet for this attempt — once tokens have started arriving, a
    // stream error is terminal (never silently retry mid-answer). The
    // (still-empty, since nothing streamed) assistant placeholder from the
    // failed attempt is dropped and re-added *after* the switch notice, so
    // `updateLastMessage` below keeps targeting the placeholder rather than
    // clobbering the notice that was just inserted after it.
    while (attempt.streamError !== null && !attempt.contentStarted && sequenceIndex + 1 < sequence.length) {
      if (attempt.usage) {
        durable.recorder?.recordUsage(attempt.usage.promptTokens, attempt.usage.completionTokens);
      }
      await durable.recorder
        ?.fail(new Error(`Target failed before output: ${attempt.streamError}`), true)
        .catch((error) => console.error('Failed to close failed-over durable run', error));
      sequenceIndex += 1;
      target = sequence[sequenceIndex];
      effort = effortForTarget(target);
      durable.recorder = await startDurableRecorder(
        target,
        `${turnId}-failover-${sequenceIndex}`,
      ).catch((error) => {
        console.error('Failed to start failover durable run', error);
        return null;
      });
      if (durable.recorder) {
        const controller = turnControllers.get(sessionId);
        if (controller) registerDurableController(durable.recorder.runId, controller, sessionId);
      }
      applyTargetSwitch(target);
      removeLastMessage();
      addMessage({
        role: 'system',
        content: `${SWITCH_NOTE_PREFIX} Switched to ${targetLabel(target)} after the previous provider didn't respond.`,
      });
      const failoverTargetBeforePrivacy = target;
      const preparedFailoverWire = await privacyGateWireForTarget(
        failoverTargetBeforePrivacy,
        wireHistoryFor(failoverTargetBeforePrivacy),
      );
      if (!preparedFailoverWire) return;
      target = preparedFailoverWire.target;
      const failoverWireHistory =
        target === failoverTargetBeforePrivacy
          ? preparedFailoverWire.messages
          : wireHistoryFor(target);
      addMessage({ role: 'assistant', content: '' });
      attempt = await attemptStream(
        target,
        failoverWireHistory,
        toolsForTurn,
        signal,
        effort,
        sessionId,
        (content) => updateLastMessage({ content }),
        true,
        undefined,
        durable.recorder?.runId,
        true,
        { preGated: true },
      );
    }
    // The corrective instruction is wire-only and belongs to exactly this
    // model round trip (including any failover attempts above). Never persist
    // it into the transcript or let it bias later turns.
    mutationCorrectionPending = false;

    const { content, toolCalls, streamError } = attempt;
    const messageId = `message-${turnId}-${iteration}`;
    durable.recorder?.recordModelOutput(messageId, content);
    if (attempt.usage) {
      durable.recorder?.recordUsage(attempt.usage.promptTokens, attempt.usage.completionTokens);
    }

    if (streamError !== null) {
      durable.failure = streamError;
      updateLastMessage({
        content:
          enforceMutation && !mutationSucceeded
            ? `No files changed. The requested workspace edit could not be completed because tool calling failed: ${streamError}`
            : content.length > 0
              ? `${content}\n\n[Error: ${streamError}]`
              : `[Error: ${streamError}]`,
      });
      return;
    }

    if (toolCalls.length === 0) {
      if (signal?.aborted && content.length === 0) {
        // Stop button fired before any content streamed in — drop the empty
        // placeholder instead of leaving a stuck "typing" bubble behind.
        removeLastMessage();
        return;
      }
      const mutationAction = mutationPlainResponseAction(
        enforceMutation,
        mutationSucceeded,
        mutationCorrectiveRetryUsed,
        unresolvedMutationFailures.size > 0,
      );
      if (mutationAction === 'retry') {
        // A chat/code answer is not evidence that the workspace changed.
        // Remove it from the visible transcript and give the same
        // tool-capable turn exactly one more chance with a wire-only system
        // correction. The booleans make the retry strictly bounded and keep
        // the correction out of future turns.
        removeLastMessage();
        mutationCorrectionPending = true;
        mutationCorrectiveRetryUsed = true;
        continue;
      }
      if (mutationAction === 'fail') {
        const unresolvedFailure = unresolvedMutationFailures.values().next().value as string | undefined;
        const failureMessage = unresolvedFailure !== undefined
          ? mutationAttemptFailureMessage(mutationSucceeded, unresolvedFailure)
          : WORKSPACE_MUTATION_FAILURE;
        durable.failure = failureMessage;
        // Replace the plain response in place so it cannot look like a
        // completed edit after either a failed tool call or the bounded retry.
        updateLastMessage({ content: failureMessage });
        return;
      }
      // The model gave a plain answer with no further tool requests — this
      // turn's natural exit point. Run the workspace's configured
      // verification commands (if any files were mutated and the user
      // hasn't hit Stop) before returning — see `runVerificationPhase`'s doc
      // comment for exactly what gates this.
      if (!signal?.aborted && mutatedFiles.size > 0) {
        const verificationStartedAt = Date.now();
        const failure = await runVerificationPhase(sessionId, turnId, addMessage, signal);
        if (settings.verifyEnabled && !signal?.aborted) {
          durable.recorder?.recordVerification(
            failure?.label ?? 'Workspace verification',
            failure === null,
            failure === null
              ? 'Configured verification completed without a reported failure.'
              : `Exit ${failure.code ?? 'timeout'}: ${failure.output}`,
            Date.now() - verificationStartedAt,
          );
        }
        // A command failed and there's a feed-back round left to spend —
        // append one fix instruction and send the loop around again instead
        // of returning. `mutatedFiles` is cleared so only edits made in
        // response to *this* failure trigger the next verification pass;
        // `signal?.aborted` is re-checked since `runVerificationPhase` can
        // return early (rather than run every command) once Stop fires
        // mid-phase — see its doc comment.
        if (failure !== null && !signal?.aborted && shouldFeedBackVerifyFailure(failure, verifyRound, settings.verifyMaxRounds)) {
          verifyRound += 1;
          mutatedFiles.clear();
          addMessage({
            role: 'system',
            content: `${VERIFY_FIX_NOTE_PREFIX} The verification command "${failure.label}" failed (exit ${failure.code ?? 'timeout'}). Fix the reported problems, then stop.\n${failure.output}`,
          });
          continue;
        }
      }
      return;
    }

    // Record the tool calls on the assistant message that requested them
    // before executing them and feeding results back.
    updateLastMessage({ content, tool_calls: toolCalls });

    // Executes every call in this round — `task` calls run concurrently
    // (bounded by `settings.maxConcurrentSubagents`), everything else stays
    // sequential — see `runToolCallsForRound`'s own doc comment for why, and
    // for the order-preservation guarantee the rest of this loop depends on.
    // One shared group id for this round's parallel `task` calls (two or
    // more — a lone one stays ungrouped), so the Background-tasks drawer can
    // render them as one card. A fresh UUID rather than any tool-call id:
    // provider-fallback ids (`call_0`) repeat across rounds and would merge
    // unrelated groups. See `SubagentContext.taskGroupId`.
    const roundTaskCallCount = toolCalls.filter((call) => call.function.name === 'task').length;
    const taskGroupId = roundTaskCallCount > 1 ? crypto.randomUUID() : undefined;
    const results = await runToolCallsForRound(toolCalls, settings.maxConcurrentSubagents, async (toolCall) => {
      const toolStartedAt = Date.now();
      const recorder = durable.recorder;
      await recorder?.recordToolProposed(
        toolCall.id,
        toolCall.function.name,
        toolCall.function.arguments ?? '{}',
      );
      recorder?.recordToolStarted(toolCall.id);
      const finishObservedTool = async (result: string): Promise<string> => {
        await recorder?.recordToolFinished(
          toolCall.id,
          result,
          Date.now() - toolStartedAt,
          result === CANCELLED_TOOL_RESULT || signal?.aborted === true,
        );
        // Failing results are also this turn's evidence about any learned
        // skill it invoked (see `recordSkillUses`). Classified the same way
        // `durableRun.ts`'s `resultOutcome` classifies it, and bounded so a
        // pathological round cannot grow the record without limit.
        if (result !== CANCELLED_TOOL_RESULT && durable.toolFailures.length < MAX_RECORDED_TOOL_FAILURES) {
          try {
            const parsed = JSON.parse(result) as { error?: unknown };
            if (parsed && typeof parsed === 'object' && parsed.error) {
              durable.toolFailures.push(`${toolCall.function.name}: ${String(parsed.error).slice(0, 200)}`);
            }
          } catch {
            // A plain-text tool result is a success — nothing to record.
          }
        }
        return result;
      };
      // Reject (without executing) any call whose name wasn't actually
      // offered to the model this turn — e.g. `remember` after
      // `memoryEnabled` was turned off, or any other tool a local/quantized
      // model hallucinates outside the schema it was given. `toolsForSettings`
      // only shapes what's *offered*; this is the enforcement point that
      // makes that toggle an actual authorization boundary rather than a
      // polite suggestion the model can ignore. Still gets a result message,
      // same invariant as the cancelled-call path below.
      if (!isToolCallAllowed(toolCall, toolsForTurn)) {
        return finishObservedTool(
          stringifyToolError(new Error(`Tool "${toolCall.function.name}" was not offered this turn and was not executed.`)),
        );
      }

      // Once the Stop button has fired, remaining calls are not executed —
      // but every one still gets a (cancelled) result message, so the
      // transcript never carries a tool_calls entry without its results
      // (several providers reject such a history on the next turn).
      // Built fresh per call (not hoisted once before the loop) so a `task`
      // call always sees the CURRENT `target` — a failover switch earlier in
      // this same iteration (or, in principle, an auto-vision switch on the
      // next one) must never leave a subagent resolving a target the parent
      // has since moved off of. See `SubagentContext`'s doc comment in
      // `turnEngine.ts`.
      if (signal?.aborted) return finishObservedTool(CANCELLED_TOOL_RESULT);
      // Status-line activity label — with concurrent `task` calls the most
      // recently started one wins, which is fine for a one-word indicator.
      useTurnStatusStore.getState().setActivity(sessionId, toolCall.function.name);
      // `risk`/`onMutatedPath` thread THIS turn's own risk-annotation context
      // and mutated-file tracking down into a `code`-profile child's own
      // write_file/edit_file/run_shell calls — without these, a subagent's
      // mutations would silently skip risk classification (even when the
      // parent turn has it enabled) and never trip `runVerificationPhase`
      // (since `mutatedFiles` below is otherwise only ever populated from
      // this round's own top-level `toolCalls`, which for a `task` call is
      // just the single `task` entry, never the child's nested writes).
      const subagentContext: SubagentContext = {
        sessionId,
        runId: durable.recorder?.runId,
        taskGroupId,
        target,
        effort,
        risk: riskAnnotation,
        // The child's own dispatch decision, onto this turn's run. A subagent
        // has no durable run of its own — it already borrows this one's id for
        // permission and cancellation audit.
        onRoutingDecision: (decision) => durable.recorder?.recordRoutingDecision(decision),
        onMutatedPath: (path) => {
          mutatedFiles.add(path);
          mutationSucceeded = true;
          unresolvedMutationFailures.delete(path);
        },
        onMutationFailure: (path, reason, childToolCallId) => {
          unresolvedMutationFailures.set(
            path ?? `subagent-tool-call:${childToolCallId}`,
            reason,
          );
        },
      };
      const result = await executeToolCall(
        toolCall,
        checkpointId,
        durable.recorder?.runId ?? turnId,
        mcpRegistry,
        signal,
        riskAnnotation,
        attachedStackNames,
        subagentContext,
        undefined,
        skillToolContext,
        sessionId,
        undefined,
        extensionRegistry,
      );
      return finishObservedTool(result);
    });
    // Control returns to the model for the next round — back to "thinking".
    useTurnStatusStore.getState().setActivity(sessionId, '');

    for (let toolCallIndex = 0; toolCallIndex < toolCalls.length; toolCallIndex++) {
      const toolCall = toolCalls[toolCallIndex];
      const resultContent = results[toolCallIndex];
      const modelResultContent = protectToolResult(
        toolCall.function.name,
        resultContent,
        mcpRegistry.has(toolCall.function.name) || extensionRegistry.has(toolCall.function.name),
      );
      const toolMessage: ChatMessage = {
        role: 'tool',
        tool_call_id: toolCall.id,
        content: modelResultContent,
      };
      addMessage(toolMessage);

      // Track this turn's file mutations for `runVerificationPhase` at the
      // loop's eventual exit — only for calls that actually ran (not
      // cancelled, not rejected above) and actually succeeded (the
      // "Wrote…"/"Edited…" string shape, not `{"error": ...}"`).
      if (
        !signal?.aborted
        && (toolCall.function.name === 'write_file' || toolCall.function.name === 'edit_file')
      ) {
        if (isSuccessfulMutationResult(resultContent)) {
          const path = toolCallPathArg(toolCall);
          if (path) {
            mutatedFiles.add(path);
            mutationSucceeded = true;
            unresolvedMutationFailures.delete(path);
          }
        } else {
          const path = toolCallPathArg(toolCall);
          unresolvedMutationFailures.set(
            path ?? `tool-call:${toolCall.id}`,
            mutationToolFailureReason(resultContent)
              ?? 'The file-mutation tool returned an error.',
          );
        }
      }

      // A successful `remember` gets its own transcript notice (with a
      // Forget button — see MessageList.tsx's MemoryRow), cloned from how
      // checkpoint_end's summary becomes a checkpoint notice. rulesStore is
      // refreshed right after so later iterations of THIS turn already see
      // the new fact in the system prompt, not just the next turn.
      if (toolCall.function.name === 'remember') {
        const fact = parseRememberedFact(resultContent);
        if (fact) {
          addMessage({ role: 'system', content: formatMemoryNotice({ id: fact.id, text: fact.text }) });
          await useRulesStore.getState().refresh();
        }
      }

      // A successful `present_plan` gets its own transcript notice (rendered
      // as a `PlanCard` with Approve/Keep-planning buttons — see
      // `MessageList.tsx`), cloned from the `remember` notice pattern just
      // above. Gated on `resultContent === PRESENT_PLAN_RESULT` — the exact
      // literal `executeToolCall`/`turnEngine.ts` returns for this tool and
      // nothing else — rather than just the tool name, so a cancelled call
      // (`CANCELLED_TOOL_RESULT`) or a call rejected by `isToolCallAllowed`
      // above (which `continue`s before reaching here at all) never produces
      // a plan card for a plan that was never actually presented.
      if (toolCall.function.name === 'present_plan' && resultContent === PRESENT_PLAN_RESULT) {
        const planArgs = toolCallPlanArgs(toolCall);
        if (planArgs) {
          addMessage({
            role: 'system',
            content: formatPlanNotice({
              id: crypto.randomUUID(),
              title: planArgs.title,
              plan: planArgs.plan,
              openQuestions: planArgs.openQuestions,
              status: 'proposed',
            }),
          });
        }
      }
    }

    if (signal) await parkHere();
    if (signal?.aborted) return;

    // Loop again: the model gets the tool results appended to its history.
  }

  // Safety cap reached: the model kept requesting tools without ever
  // settling on a final answer. Surface this clearly instead of looping
  // forever or silently truncating.
  if (enforceMutation && (!mutationSucceeded || unresolvedMutationFailures.size > 0)) {
    const unresolvedFailure = unresolvedMutationFailures.values().next().value as string | undefined;
    const failureMessage = unresolvedFailure !== undefined
      ? mutationAttemptFailureMessage(mutationSucceeded, unresolvedFailure)
      : WORKSPACE_MUTATION_FAILURE;
    durable.failure = failureMessage;
    addMessage({ role: 'assistant', content: failureMessage });
    return;
  }
  durable.failure = `Reached the safety limit of ${MAX_ITERATIONS} tool-calling iterations.`;
  addMessage({
    role: 'assistant',
    content: `Stopped after reaching the safety limit of ${MAX_ITERATIONS} tool-calling iterations without a final answer.`,
  });
}
