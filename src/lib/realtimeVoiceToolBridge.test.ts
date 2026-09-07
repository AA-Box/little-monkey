import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { ToolDef } from './llamaClient';
import { appendRealtimeTranscript, executeRealtimeToolCall, type RealtimeToolSurface } from './realtimeVoiceToolBridge';
import { useSessionStore } from '../store/sessionStore';

const tool: ToolDef = {
  type: 'function',
  function: { name: 'read_file', description: 'Read', parameters: { type: 'object', properties: {} } },
};

const surface: RealtimeToolSurface = {
  tools: [tool],
  mcpRegistry: new Map(),
  extensionRegistry: new Map(),
  attachedStackNames: [],
};

beforeEach(() => {
  useSessionStore.setState({
    sessions: [{
      id: 'chat', title: 'New session', messages: [], createdAt: 1, updatedAt: 1,
      pinned: false, unread: false, archived: false, groupId: null, modelTarget: null,
      comparisonBranch: null, workspacePath: null, personaId: null, attachedStackIds: [],
      docChatMode: false, subagentRuns: {}, subagentRunStats: {},
    }],
    activeSessionId: 'chat',
  } as never);
});

describe('realtime tool bridge', () => {
  it('persists finalized transcripts once across event replay', () => {
    expect(appendRealtimeTranscript('chat', 'rv', 'u1', 'e1', 'user', 'hello')).toBe(true);
    expect(appendRealtimeTranscript('chat', 'rv', 'u1', 'e2', 'user', 'hello')).toBe(false);
    expect(useSessionStore.getState().sessions[0].messages).toHaveLength(1);
  });

  it.each([
    ['allowed', '{"ok":true}'],
    ['refused', '{"error":"Permission denied by user"}'],
    ['tool error', '{"error":"File not found"}'],
  ])('closes an %s tool call with the normal executor result', async (_label, expected) => {
    const execute = vi.fn(async () => expected);
    const result = await executeRealtimeToolCall(
      { id: 'call-1', itemId: 'item-1', name: 'read_file', arguments: '{}' },
      { chatSessionId: 'chat', realtimeSessionId: 'rv', surface, execute: execute as never },
    );
    expect(result).toBe(expected);
    const messages = useSessionStore.getState().sessions[0].messages;
    expect(messages.map((message) => message.role)).toEqual(['assistant', 'tool']);
    expect(messages[1].tool_call_id).toBe('call-1');
  });

  it('turns an unexpected executor failure into a tool result and never replays it', async () => {
    const execute = vi.fn(async () => { throw new Error('malformed arguments'); });
    const call = { id: 'call-2', itemId: 'item-2', name: 'read_file', arguments: '{' };
    const first = await executeRealtimeToolCall(call, {
      chatSessionId: 'chat', realtimeSessionId: 'rv', surface, execute: execute as never,
    });
    const second = await executeRealtimeToolCall(call, {
      chatSessionId: 'chat', realtimeSessionId: 'rv', surface, execute: execute as never,
    });
    expect(JSON.parse(first)).toEqual({ error: 'malformed arguments' });
    expect(second).toBe(first);
    expect(execute).toHaveBeenCalledTimes(1);
  });

  it('surfaces the normal approval wait without creating a second decision path', async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const waiting: boolean[] = [];
    const execute = vi.fn(async () => {
      await gate;
      return '{"ok":true}';
    });
    const pending = executeRealtimeToolCall(
      { id: 'call-approval', itemId: 'item-approval', name: 'read_file', arguments: '{}' },
      {
        chatSessionId: 'chat', realtimeSessionId: 'rv', surface,
        execute: execute as never,
        onAwaitingApproval: (value) => waiting.push(value),
      },
    );
    await Promise.resolve();
    expect(waiting).toEqual([true]);
    release();
    await pending;
    expect(waiting).toEqual([true, false]);
  });
});
