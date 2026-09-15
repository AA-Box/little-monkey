import { toolsForMode, toolsForSettings } from './agentLoop';
import { executableExtensionToolDefs, type ExtensionToolRegistry } from './executableExtensionTools';
import type { ChatMessage, ToolCall, ToolDef } from './llamaClient';
import { mcpToolDefs, type McpToolRegistry } from './mcpTools';
import type { RealtimeVoiceToolCall } from './realtimeVoice';
import { executeToolCall } from './turnEngine';
import type { DurableRunRecorder } from './durableRun';
import { buildTools, toolsForWorkspace } from './tools';
import { usePermissionStore } from '../store/permissionStore';
import { useSessionStore } from '../store/sessionStore';
import { useSettingsStore } from '../store/settingsStore';
import { primaryRoot, useWorkspaceStore } from '../store/workspaceStore';

export interface RealtimeToolSurface {
  tools: ToolDef[];
  mcpRegistry: McpToolRegistry;
  extensionRegistry: ExtensionToolRegistry;
  attachedStackNames: string[];
}

export async function buildRealtimeToolSurface(attachedStackNames: string[]): Promise<RealtimeToolSurface> {
  const settings = useSettingsStore.getState();
  const permissionMode = usePermissionStore.getState().mode;
  const hasWorkspace = primaryRoot(useWorkspaceStore.getState().roots) !== null;
  const mcp = mcpToolDefs();
  const extensions = await executableExtensionToolDefs();
  let tools = [...buildTools(attachedStackNames), ...mcp.defs, ...extensions.defs];
  tools = toolsForWorkspace(tools, hasWorkspace);
  tools = toolsForMode(tools, permissionMode);
  // A realtime voice turn intentionally does not create a second agent loop,
  // so nested agents/skills are not offered. Every direct, MCP, extension,
  // memory, web, and desktop-control call still uses the normal executor.
  tools = toolsForSettings(
    tools,
    settings.memoryEnabled,
    settings.webToolsEnabled,
    false,
    false,
    false,
    false,
    false,
    settings.desktopControlEnabled,
  );
  return { tools, mcpRegistry: mcp.registry, extensionRegistry: extensions.registry, attachedStackNames };
}

function hasRealtimeItem(sessionId: string, itemId: string, kind: string): boolean {
  const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === sessionId);
  return session?.messages.some((message) =>
    message.realtime?.itemId === itemId && message.realtime.kind === kind,
  ) ?? false;
}

function canonicalArguments(raw: string): string {
  // A call with no arguments arrives as '' or as '{}' depending on what the
  // provider put in the item, and those must hash to the same operation.
  if (raw.trim() === '') return '{}';
  try {
    const sort = (value: unknown): unknown => {
      if (Array.isArray(value)) return value.map(sort);
      if (value && typeof value === 'object') {
        return Object.fromEntries(
          Object.entries(value as Record<string, unknown>)
            .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
            .map(([key, entry]) => [key, sort(entry)]),
        );
      }
      return value;
    };
    return JSON.stringify(sort(JSON.parse(raw)));
  } catch {
    // Malformed arguments still identify one operation; the executor is the
    // component that rejects them, and it must reject them exactly once.
    return raw;
  }
}

/** Host-side identity of one tool execution: the tool plus its arguments,
 * independent of any provider session, response, item, or call id. Argument
 * key order is normalized so a re-serialized reissue matches. */
export async function realtimeCallKey(name: string, args: string): Promise<string> {
  const identity = new TextEncoder().encode(`${name}\n${canonicalArguments(args)}`);
  const digest = await crypto.subtle.digest('SHA-256', identity);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function realtimeMessages(sessionId: string) {
  const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === sessionId);
  return session?.messages ?? [];
}

/** Whether a persisted row belongs to the same operation carried out earlier in
 * this spoken turn by a *previous* provider conversation.
 *
 * The `sessionId` half is what keeps the reconnect protection from swallowing
 * ordinary work: inside one provider conversation the model may legitimately
 * call the same tool with the same arguments twice — read a file it just wrote,
 * re-run a check — and only the provider item id may deduplicate there. A
 * reconnect always brings a fresh `rv_` session id, which is exactly the case
 * the host-side identity exists for. */
function reissuedFromAnotherSession(
  realtime: NonNullable<ChatMessage['realtime']>,
  realtimeSessionId: string,
  voiceTurnId: string | undefined,
  callKey: string,
): boolean {
  return Boolean(voiceTurnId)
    && realtime.voiceTurnId === voiceTurnId
    && realtime.callKey === callKey
    && realtime.sessionId !== realtimeSessionId;
}

/** A tool result already persisted for this operation, whether it was recorded
 * against the same provider item or against the same host-side call key in an
 * earlier provider session of the same spoken turn. */
function persistedToolResult(
  chatSessionId: string,
  call: RealtimeVoiceToolCall,
  options: RealtimeToolBridgeOptions,
  callKey: string,
): string | null {
  for (const message of realtimeMessages(chatSessionId)) {
    const realtime = message.realtime;
    if (realtime?.kind !== 'tool_result') continue;
    const sameProviderItem = realtime.itemId === call.itemId;
    if (!sameProviderItem
      && !reissuedFromAnotherSession(realtime, options.realtimeSessionId, options.voiceTurnId, callKey)) continue;
    return typeof message.content === 'string' ? message.content : '{"error":"Duplicate tool call ignored."}';
  }
  return null;
}

/** True when an earlier provider session in this spoken turn dispatched this
 * exact operation and no result was ever recorded for it — the connection
 * dropped between dispatch and result, so whether it took effect is unknown.
 *
 * This fails closed for every tool rather than consulting a read-only/mutating
 * classification. The frontend has no per-tool idempotency contract that is
 * complete enough to bet a side effect on, and the cost of failing closed is
 * one refusal the operator resolves by asking again — a fresh spoken turn gets
 * a fresh identity and runs normally. */
function startedWithUnknownOutcome(
  chatSessionId: string,
  call: RealtimeVoiceToolCall,
  options: RealtimeToolBridgeOptions,
  callKey: string,
): boolean {
  if (!options.voiceTurnId) return false;
  const messages = realtimeMessages(chatSessionId);
  const dispatched = messages.some((message) => message.realtime?.kind === 'tool_call'
    && message.realtime.itemId !== call.itemId
    && reissuedFromAnotherSession(message.realtime, options.realtimeSessionId, options.voiceTurnId, callKey));
  if (!dispatched) return false;
  return !messages.some((message) => message.realtime?.kind === 'tool_result'
    && message.realtime.voiceTurnId === options.voiceTurnId
    && message.realtime.callKey === callKey);
}

export function appendRealtimeTranscript(
  chatSessionId: string,
  realtimeSessionId: string,
  itemId: string,
  eventId: string,
  role: 'user' | 'assistant',
  text: string,
  /** Stamped so a finished spoken answer closes the turn for
   * `recoverRealtimeVoiceTurnId`; without it a later session would treat the
   * completed turn as still unresolved. */
  voiceTurnId?: string,
): boolean {
  const clean = text.trim();
  const kind = role === 'user' ? 'input_transcript' : 'output_transcript';
  if (!clean || hasRealtimeItem(chatSessionId, itemId, kind)) return false;
  useSessionStore.getState().addMessage(chatSessionId, {
    role,
    content: clean,
    realtime: { sessionId: realtimeSessionId, itemId, eventId, ...(voiceTurnId ? { voiceTurnId } : {}), kind },
  });
  return true;
}

export interface RealtimeToolBridgeOptions {
  chatSessionId: string;
  realtimeSessionId: string;
  surface: RealtimeToolSurface;
  signal?: AbortSignal;
  onAwaitingApproval?: (waiting: boolean) => void;
  execute?: typeof executeToolCall;
  recorder?: DurableRunRecorder | null;
  /** Durable run/permission identity for the current spoken turn. */
  turnId?: string;
  /** Identity of the spoken turn that survives a reconnect into a brand-new
   * provider session. Without it, dedupe degrades to provider item ids only. */
  voiceTurnId?: string;
  checkpointId?: string | null;
}

/** Executes one provider call through the exact normal tool boundary.
 *
 * Every execution is persisted against two identities: the provider item id,
 * which makes a replayed provider event harmless, and `voiceTurnId` + a digest
 * of tool name and canonical arguments, which makes a *reconnect* harmless.
 * The reconnected session is a different provider conversation with different
 * item ids, so item ids alone cannot tell that the model is asking for an
 * operation the host already performed. */
export async function executeRealtimeToolCall(
  call: RealtimeVoiceToolCall,
  options: RealtimeToolBridgeOptions,
): Promise<string> {
  const callKey = await realtimeCallKey(call.name, call.arguments);
  const persisted = persistedToolResult(options.chatSessionId, call, options, callKey);
  if (persisted !== null) return persisted;
  if (startedWithUnknownOutcome(options.chatSessionId, call, options, callKey)) {
    return JSON.stringify({
      error: `${call.name} was already started in this voice turn and its outcome is unknown. It was not run a second time; ask again to retry it deliberately.`,
    });
  }
  const turnId = options.turnId ?? `realtime:${options.realtimeSessionId}:${call.itemId}`;
  const toolCall: ToolCall = {
    id: call.id,
    type: 'function',
    function: { name: call.name, arguments: call.arguments },
  };
  if (!hasRealtimeItem(options.chatSessionId, call.itemId, 'tool_call')) {
    useSessionStore.getState().addMessage(options.chatSessionId, {
      role: 'assistant',
      content: '',
      tool_calls: [toolCall],
      realtime: {
        sessionId: options.realtimeSessionId,
        itemId: call.itemId,
        turnId,
        ...(options.voiceTurnId ? { voiceTurnId: options.voiceTurnId } : {}),
        callKey,
        kind: 'tool_call',
      },
    });
  }
  options.onAwaitingApproval?.(true);
  const startedAt = performance.now();
  await options.recorder?.recordToolProposed(call.id, call.name, call.arguments).catch(() => undefined);
  options.recorder?.recordToolStarted(call.id);
  const executor = options.execute ?? executeToolCall;
  let result: string;
  try {
    result = await executor(
      toolCall,
      options.checkpointId ?? null,
      turnId,
      options.surface.mcpRegistry,
      options.signal,
      undefined,
      options.surface.attachedStackNames,
      undefined,
      'Realtime voice',
      undefined,
      options.chatSessionId,
      undefined,
      options.surface.extensionRegistry,
      undefined,
      {
        toolDefinitions: options.surface.tools,
        isToolAvailable: (name) => options.surface.tools.some((tool) => tool.function.name === name),
      },
    );
  } catch (error) {
    // Defense in depth: executeToolCall normally converts every failure into
    // a result, because a provider function call must always be closed.
    result = JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
  } finally {
    options.onAwaitingApproval?.(false);
  }
  await options.recorder?.recordToolFinished(
    call.id,
    result,
    Math.max(0, Math.round(performance.now() - startedAt)),
    options.signal?.aborted ?? false,
  ).catch(() => undefined);
  if (!hasRealtimeItem(options.chatSessionId, call.itemId, 'tool_result')) {
    useSessionStore.getState().addMessage(options.chatSessionId, {
      role: 'tool',
      tool_call_id: call.id,
      content: result,
      realtime: {
        sessionId: options.realtimeSessionId,
        itemId: call.itemId,
        turnId,
        ...(options.voiceTurnId ? { voiceTurnId: options.voiceTurnId } : {}),
        callKey,
        kind: 'tool_result',
      },
    });
  }
  return result;
}
