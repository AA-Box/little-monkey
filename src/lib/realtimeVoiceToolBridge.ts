import { toolsForMode, toolsForSettings } from './agentLoop';
import { executableExtensionToolDefs, type ExtensionToolRegistry } from './executableExtensionTools';
import type { ToolCall, ToolDef } from './llamaClient';
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

export function appendRealtimeTranscript(
  chatSessionId: string,
  realtimeSessionId: string,
  itemId: string,
  eventId: string,
  role: 'user' | 'assistant',
  text: string,
): boolean {
  const clean = text.trim();
  const kind = role === 'user' ? 'input_transcript' : 'output_transcript';
  if (!clean || hasRealtimeItem(chatSessionId, itemId, kind)) return false;
  useSessionStore.getState().addMessage(chatSessionId, {
    role,
    content: clean,
    realtime: { sessionId: realtimeSessionId, itemId, eventId, kind },
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
  checkpointId?: string | null;
}

/** Executes one provider call through the exact normal tool boundary. The
 * transcript entries are written before/after execution and keyed by the
 * provider item id so reconnect replay is harmless. */
export async function executeRealtimeToolCall(
  call: RealtimeVoiceToolCall,
  options: RealtimeToolBridgeOptions,
): Promise<string> {
  if (hasRealtimeItem(options.chatSessionId, call.itemId, 'tool_result')) {
    const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === options.chatSessionId);
    const existing = session?.messages.find((message) =>
      message.realtime?.itemId === call.itemId && message.realtime.kind === 'tool_result',
    );
    return typeof existing?.content === 'string' ? existing.content : '{"error":"Duplicate tool call ignored."}';
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
        kind: 'tool_result',
      },
    });
  }
  return result;
}
