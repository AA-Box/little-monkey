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
import { invoke } from '@tauri-apps/api/core';
import { streamChat, textContent } from './llamaClient';
import type { ChatContentPart, ChatMessage, StreamEvent, ToolCall, ToolDef } from './llamaClient';
import { streamProviderChat } from './providerClient';
import { TOOLS } from './tools';
import { formatMcpCallToolResult, mcpToolDefs, resolveMcpToolName, type McpCallToolResult, type McpToolRegistry } from './mcpTools';
import { isVisionCapableOllamaModel, isVisionCapableProviderModel } from './visionModels';
import { recordRequest } from './rateLimitTracker';
import { applyContextCompaction, renderForSummary, shouldTrim } from './contextTrimmer';
import {
  composeReferencedText,
  extractMentionPaths,
  formatDirListing,
  type DirEntry,
  type ResolvedTextReference,
} from './mentions';
import { currentSystemPrompt } from './systemPrompt';
import { sessionMessages, useSessionStore } from '../store/sessionStore';
import { getActiveChatTarget, useModelStore } from '../store/modelStore';
import { useUsageStore } from '../store/usageStore';
import { useSettingsStore } from '../store/settingsStore';
import { useRulesStore } from '../store/rulesStore';
import { useCheckpointStore } from '../store/checkpointStore';

/** Hard cap on model/tool round trips for a single call to runAgentTurn. */
const MAX_ITERATIONS = 25;

/** Mirrors `LlamaState::default()` in src-tauri/src/llama.rs. */
const DEFAULT_LLAMA_PORT = 8090;

/** Prefix identifying a synthetic model-switch notice (auto-failover or
 * vision auto-switch) inserted into the transcript — mirrors
 * `contextTrimmer.ts`'s `COMPACTION_MARKER_PREFIX` pattern so `MessageList`
 * can recognize and render both kinds of system-role notice distinctly from
 * a real (currently nonexistent, but defensively still hidden) system
 * message. */
export const SWITCH_NOTE_PREFIX = '[Model switch]';

export function isSwitchNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(SWITCH_NOTE_PREFIX);
}

/** Prefix identifying a synthetic notice listing "@"-mentions that failed to
 * resolve this turn (typo'd path, unreadable file — see `resolveReferences`)
 * — same pattern as `SWITCH_NOTE_PREFIX`, so the user learns why the model
 * never saw the file instead of the failure being swallowed silently. */
export const MENTION_NOTE_PREFIX = '[Mentions]';

export function isMentionNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(MENTION_NOTE_PREFIX);
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

export function isCheckpointNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(CHECKPOINT_NOTE_PREFIX);
}

/** Parses a checkpoint notice's JSON payload; `null` for anything malformed. */
export function parseCheckpointNotice(message: ChatMessage): CheckpointNotice | null {
  if (!isCheckpointNotice(message)) return null;
  try {
    const parsed: unknown = JSON.parse((message.content as string).slice(CHECKPOINT_NOTE_PREFIX.length));
    if (
      parsed &&
      typeof parsed === 'object' &&
      typeof (parsed as CheckpointNotice).id === 'string' &&
      Array.isArray((parsed as CheckpointNotice).files)
    ) {
      return parsed as CheckpointNotice;
    }
  } catch {
    // Malformed payload — treat as "not a checkpoint notice".
  }
  return null;
}

/** Serializes a checkpoint notice back into message content — used both when
 * the notice is first added and when the Revert button marks it reverted. */
export function formatCheckpointNotice(notice: CheckpointNotice): string {
  return `${CHECKPOINT_NOTE_PREFIX}${JSON.stringify(notice)}`;
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

export function isMemoryNotice(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(MEMORY_NOTE_PREFIX);
}

/** Parses a memory notice's JSON payload; `null` for anything malformed. */
export function parseMemoryNotice(message: ChatMessage): MemoryNotice | null {
  if (!isMemoryNotice(message)) return null;
  try {
    const parsed: unknown = JSON.parse((message.content as string).slice(MEMORY_NOTE_PREFIX.length));
    if (
      parsed &&
      typeof parsed === 'object' &&
      typeof (parsed as MemoryNotice).id === 'string' &&
      typeof (parsed as MemoryNotice).text === 'string'
    ) {
      return parsed as MemoryNotice;
    }
  } catch {
    // Malformed payload — treat as "not a memory notice".
  }
  return null;
}

/** Serializes a memory notice back into message content — used both when the
 * notice is first added and when the Forget button marks it forgotten. */
export function formatMemoryNotice(notice: MemoryNotice): string {
  return `${MEMORY_NOTE_PREFIX}${JSON.stringify(notice)}`;
}

/**
 * Filters `remember` out of the tool list offered to the model this turn
 * when the settingsStore `memoryEnabled` toggle is off. This is the ONLY
 * effect of that toggle — rules and previously-saved facts are still
 * injected into the system prompt unconditionally (see `runAgentTurnBody`'s
 * `useRulesStore.getState().refresh()` call); turning it off stops the agent
 * from saving *new* facts on its own, it is not amnesia.
 */
export function toolsForSettings(tools: ToolDef[], memoryEnabled: boolean): ToolDef[] {
  return memoryEnabled ? tools : tools.filter((tool) => tool.function.name !== 'remember');
}

/**
 * Whether `toolCall` was actually among the tools offered to the model this
 * turn. `toolsForSettings` only shapes the *schema* sent to the model (e.g.
 * dropping `remember` when `memoryEnabled` is off) — nothing downstream of
 * that used to check it, so a model that still emitted a disabled or
 * hallucinated tool call (a real risk with local/quantized models that don't
 * strictly respect the offered tool schema) would have it executed anyway.
 * The tool-calling loop calls this before dispatch and rejects (without
 * executing) anything that fails it.
 */
export function isToolCallAllowed(toolCall: ToolCall, toolsForTurn: ToolDef[]): boolean {
  return toolsForTurn.some((tool) => tool.function.name === toolCall.function.name);
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

/** Shape returned by the `llama_status` Tauri command. */
interface LlamaStatusPayload {
  status: 'stopped' | 'starting' | 'ready' | 'error';
  port: number;
  model_path: string | null;
}

/**
 * Resolves the base URL of the locally running llama-server by asking the
 * Rust backend for its current status, which is the source of truth for the
 * port it actually bound to. Falls back to the documented default port if
 * the status can't be read for any reason (e.g. server not started yet),
 * so a subsequent request simply fails with a clear connection error rather
 * than this function throwing before the user ever sees why.
 */
async function resolveBaseUrl(): Promise<string> {
  try {
    const status = await invoke<LlamaStatusPayload>('llama_status');
    const port =
      typeof status?.port === 'number' && Number.isFinite(status.port) && status.port > 0
        ? status.port
        : DEFAULT_LLAMA_PORT;
    return `http://127.0.0.1:${port}`;
  } catch {
    return `http://127.0.0.1:${DEFAULT_LLAMA_PORT}`;
  }
}

/** Where a turn's requests should go. Local llama.cpp and Ollama are kept
 * distinct (rather than a single generic "direct fetch" kind) so
 * failover/vision-switch logic can tell exactly which store setter
 * (`useOllamaModel` vs `useProviderModel`) to call when it picks a
 * different target — both still stream via the same `streamChat` transport. */
export type ResolvedTarget =
  | { kind: 'local'; baseUrl: string }
  | { kind: 'ollama'; baseUrl: string; model: string }
  | { kind: 'provider'; providerId: string; model: string };

/**
 * Resolves the active chat target into exactly what's needed to stream a
 * turn. Cloud providers go through the Rust-proxied `streamProviderChat`
 * (its API key lives in the OS keychain, never here); local llama.cpp and
 * the unauthenticated local Ollama daemon both use the direct-`fetch`
 * `streamChat` path.
 */
async function resolveTarget(): Promise<ResolvedTarget> {
  const target = getActiveChatTarget();

  if (target.kind === 'provider') {
    if (!target.providerId || !target.model) {
      throw new Error('No AI provider model selected');
    }
    return { kind: 'provider', providerId: target.providerId, model: target.model };
  }

  if (target.kind === 'ollama') {
    if (!target.model) {
      throw new Error('No Ollama model selected');
    }
    return { kind: 'ollama', baseUrl: target.baseUrl, model: target.model };
  }

  const baseUrl = await resolveBaseUrl();
  return { kind: 'local', baseUrl };
}

/** Human-readable label for a switch notice. */
function targetLabel(target: ResolvedTarget): string {
  if (target.kind === 'provider') return `${target.providerId} (${target.model})`;
  if (target.kind === 'ollama') return `Ollama (${target.model})`;
  return 'the local model';
}

/** Applies `target` as the app's active chat target — the same store setters a manual switch in the UI would call, which is exactly what makes the switch "sticky" across subsequent turns (session affinity) with no separate mechanism needed. */
function applyTargetSwitch(target: ResolvedTarget): void {
  if (target.kind === 'provider') {
    useModelStore.getState().useProviderModel(target.providerId, target.model);
  } else if (target.kind === 'ollama') {
    useModelStore.getState().useOllamaModel(target.model);
  }
  // 'local' is never produced as a switch target — see buildFailoverChain/findVisionCandidate.
}

/** Whether the currently active target satisfies `requireVision` (always `true` when vision isn't required). Local llama.cpp models are never vision-capable — see `visionModels.ts`. */
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
  return false;
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

/** Searches every configured target (cloud providers first, then Ollama) for one that can see images, for the pre-turn vision auto-switch. Returns `null` if nothing qualifies. */
function findVisionCandidate(): ResolvedTarget | null {
  const chain = buildFailoverChain(true);
  if (chain.length > 0) return chain[0];

  const visionOllama = useModelStore.getState().ollamaModels.find(isVisionCapableOllamaModel);
  if (visionOllama) return { kind: 'ollama', baseUrl: 'http://127.0.0.1:11434', model: visionOllama.name };

  return null;
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

function stringifyToolError(err: unknown): string {
  const message = err instanceof Error ? err.message : typeof err === 'string' ? err : JSON.stringify(err);
  return JSON.stringify({ error: message });
}

/** An explicit attachment (from the "+" attach menu), as opposed to a text-derived "@"-mention. */
export interface AttachmentRef {
  path: string;
  isDir: boolean;
  /** Set at pick-time in `ChatWindow.tsx` for image files — its presence (alongside `dataUrl`) is what routes this attachment to the vision content-part path instead of the text-inlining path below. */
  kind?: 'image';
  /** The already-base64-encoded `data:` URL for an image attachment, read once at pick-time (see `imageAttachment.ts`) so this module never re-reads the file. */
  dataUrl?: string;
}

/** A single resolved image attachment, ready to become a `ChatContentPart`. */
interface ResolvedImage {
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
async function resolveReferences(
  text: string,
  attachments: AttachmentRef[]
): Promise<{ textRefs: ResolvedTextReference[]; images: ResolvedImage[]; unresolved: string[] }> {
  const images: ResolvedImage[] = [];
  const textAttachments: AttachmentRef[] = [];

  for (const attachment of attachments) {
    if (attachment.kind === 'image') {
      if (attachment.dataUrl) images.push({ path: attachment.path, dataUrl: attachment.dataUrl });
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

  const textRefs: ResolvedTextReference[] = [];
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
function toMessageContent(text: string, images: ResolvedImage[]): string | ChatContentPart[] {
  if (images.length === 0) return text;
  const parts: ChatContentPart[] = [{ type: 'text', text }];
  for (const image of images) parts.push({ type: 'image_url', image_url: { url: image.dataUrl } });
  return parts;
}

/** The tool-message content used for a call the user's Stop button cancelled
 * (either mid-execution, or before it ever started). A result message is
 * still recorded for every requested call so the persisted transcript never
 * contains an assistant `tool_calls` entry without its matching results —
 * several providers reject such a history outright on the next turn. */
const CANCELLED_TOOL_RESULT = JSON.stringify({ error: 'Cancelled by the user' });

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
async function executeToolCall(
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
  // model-supplied). All five commands use `rename_all = "snake_case"`, so
  // all take the snake_case key. `remember`/`web_fetch` don't take a
  // checkpoint_id (see tool_remember's/tool_web_fetch's doc comments in
  // tools.rs/web.rs — neither snapshots a workspace file), but both are still
  // permission-gated and need the turn id for that prompt (and, for
  // web_fetch, for Stop-button cancellation of the in-flight request).
  if (name === 'write_file' || name === 'edit_file' || name === 'run_shell' || name === 'remember' || name === 'web_fetch') {
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
async function attemptStream(
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
  signal?: AbortSignal
): Promise<void> {
  // Hard invariant: at most one turn per session, ever. Two turns streaming
  // into one transcript interleave their `updateLastMessage` patches and
  // corrupt it — the store's pane guards make this unreachable through the
  // UI, but the loop enforces it regardless of caller.
  if (turnControllers.has(sessionId)) {
    throw new Error('A turn is already running in this session.');
  }
  const controller = new AbortController();
  if (signal) {
    if (signal.aborted) controller.abort();
    else signal.addEventListener('abort', () => controller.abort(), { once: true });
  }
  turnControllers.set(sessionId, controller);
  useSessionStore.getState().markTurnRunning(sessionId, true);
  try {
    await runTurnGuarded(sessionId, userText, attachments, controller.signal);
  } finally {
    turnControllers.delete(sessionId);
    useSessionStore.getState().markTurnRunning(sessionId, false);
  }
}

/** `runAgentTurn` minus the per-session turn registration — the checkpoint
 * lifecycle half of the wrapper. */
async function runTurnGuarded(
  sessionId: string,
  userText: string,
  attachments: AttachmentRef[],
  signal: AbortSignal
): Promise<void> {
  // The index this turn's user message will land at — captured before
  // `addMessage` so it can anchor a later "Rewind conversation" back to the
  // state just before this turn.
  const anchorIndex = sessionMessages(sessionId).length;

  // Added as plain text first for instant feedback (resolving references
  // in the turn body does async file/image reads) — if there's at least one
  // image, it's promoted in place, right after, to a `ChatContentPart[]` so
  // the chat UI actually shows what was attached, not just what was typed.
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
  // Distinct from checkpointId (which can be null): scopes shell
  // cancellation and permission prompts to this turn on the Rust side.
  const turnId = crypto.randomUUID();
  try {
    await runAgentTurnBody(sessionId, userText, attachments, checkpointId, turnId, signal);
  } finally {
    if (checkpointId !== null) {
      const summary = await invoke<CheckpointNotice>('checkpoint_end', { id: checkpointId }).catch(() => null);
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
  signal?: AbortSignal
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
  const wireContent = textRefs.length > 0 ? toMessageContent(composedText, images) : null;
  const requireVision = images.length > 0;

  const settings = useSettingsStore.getState();

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
  const primaryTarget = await resolveTarget();
  const sequence: ResolvedTarget[] =
    settings.autoFailoverEnabled && primaryTarget.kind === 'provider'
      ? [primaryTarget, ...buildFailoverChain(requireVision).filter((c) => !(c.kind === 'provider' && c.providerId === primaryTarget.providerId && c.model === primaryTarget.model))]
      : [primaryTarget];
  let sequenceIndex = 0;
  let target = sequence[0];

  const effort = useModelStore.getState().effort;

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
  const toolsForTurn: ToolDef[] = toolsForSettings([...TOOLS, ...mcpDefs], settings.memoryEnabled);

  const sendForSummary = async (dropped: ChatMessage[]): Promise<string> => {
    const summaryMessages: ChatMessage[] = [
      {
        role: 'system',
        content:
          'Summarize the following earlier conversation concisely for another AI assistant to continue from. Preserve key facts, decisions, file paths, and code context. Reply with only the summary text.',
      },
      { role: 'user', content: renderForSummary(dropped) },
    ];
    const result = await attemptStream(target, summaryMessages, [], signal, effort, sessionId);
    if (result.streamError) throw new Error(result.streamError);
    return result.content.trim() || '(summary unavailable)';
  };

  for (let iteration = 0; iteration < MAX_ITERATIONS; iteration++) {
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
    const systemMessage: ChatMessage = { role: 'system', content: currentSystemPrompt(personaId) };

    // Build the wire payload for this request: the system prompt first, then
    // `history` — identical to the stored transcript unless this turn's user
    // message had text references to expand, in which case that one message
    // (matched by reference) is swapped for its expanded content — `history`
    // itself (and what's stored/rendered) is left untouched. No substitution
    // is needed when there were no text references (`wireContent === null`):
    // `storedUserMessage` already carries any images directly.
    const wireHistory: ChatMessage[] = [
      systemMessage,
      ...(wireContent !== null
        ? history.map((message) => (message === storedUserMessage ? { ...message, content: wireContent } : message))
        : history),
    ];

    const assistantPlaceholder: ChatMessage = { role: 'assistant', content: '' };
    addMessage(assistantPlaceholder);

    let attempt = await attemptStream(target, wireHistory, toolsForTurn, signal, effort, sessionId, (content) => updateLastMessage({ content }));

    // Failover: only ever retry a *different* target when nothing streamed
    // back yet for this attempt — once tokens have started arriving, a
    // stream error is terminal (never silently retry mid-answer). The
    // (still-empty, since nothing streamed) assistant placeholder from the
    // failed attempt is dropped and re-added *after* the switch notice, so
    // `updateLastMessage` below keeps targeting the placeholder rather than
    // clobbering the notice that was just inserted after it.
    while (attempt.streamError !== null && !attempt.contentStarted && sequenceIndex + 1 < sequence.length) {
      sequenceIndex += 1;
      target = sequence[sequenceIndex];
      applyTargetSwitch(target);
      removeLastMessage();
      addMessage({
        role: 'system',
        content: `${SWITCH_NOTE_PREFIX} Switched to ${targetLabel(target)} after the previous provider didn't respond.`,
      });
      addMessage({ role: 'assistant', content: '' });
      attempt = await attemptStream(target, wireHistory, toolsForTurn, signal, effort, sessionId, (content) => updateLastMessage({ content }));
    }

    const { content, toolCalls, streamError } = attempt;

    if (streamError !== null) {
      updateLastMessage({
        content: content.length > 0 ? `${content}\n\n[Error: ${streamError}]` : `[Error: ${streamError}]`,
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
      // The model gave a plain answer with no further tool requests — done.
      return;
    }

    // Record the tool calls on the assistant message that requested them
    // before executing them and feeding results back.
    updateLastMessage({ content, tool_calls: toolCalls });

    for (const toolCall of toolCalls) {
      // Reject (without executing) any call whose name wasn't actually
      // offered to the model this turn — e.g. `remember` after
      // `memoryEnabled` was turned off, or any other tool a local/quantized
      // model hallucinates outside the schema it was given. `toolsForSettings`
      // only shapes what's *offered*; this is the enforcement point that
      // makes that toggle an actual authorization boundary rather than a
      // polite suggestion the model can ignore. Still gets a result message,
      // same invariant as the cancelled-call path below.
      if (!isToolCallAllowed(toolCall, toolsForTurn)) {
        addMessage({
          role: 'tool',
          tool_call_id: toolCall.id,
          content: stringifyToolError(new Error(`Tool "${toolCall.function.name}" was not offered this turn and was not executed.`)),
        });
        continue;
      }

      // Once the Stop button has fired, remaining calls are not executed —
      // but every one still gets a (cancelled) result message, so the
      // transcript never carries a tool_calls entry without its results
      // (several providers reject such a history on the next turn).
      const resultContent = signal?.aborted
        ? CANCELLED_TOOL_RESULT
        : await executeToolCall(toolCall, checkpointId, turnId, mcpRegistry, signal);
      const toolMessage: ChatMessage = {
        role: 'tool',
        tool_call_id: toolCall.id,
        content: resultContent,
      };
      addMessage(toolMessage);

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
    }

    if (signal?.aborted) return;

    // Loop again: the model gets the tool results appended to its history.
  }

  // Safety cap reached: the model kept requesting tools without ever
  // settling on a final answer. Surface this clearly instead of looping
  // forever or silently truncating.
  addMessage({
    role: 'assistant',
    content: `Stopped after reaching the safety limit of ${MAX_ITERATIONS} tool-calling iterations without a final answer.`,
  });
}
