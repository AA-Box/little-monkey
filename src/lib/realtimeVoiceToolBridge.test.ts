import { beforeEach, describe, expect, it, vi } from 'vitest';

import { toWireMessages, type ToolDef } from './llamaClient';
import type { RealtimeVoiceToolCall } from './realtimeVoice';
import {
  appendRealtimeTranscript,
  executeRealtimeToolCall,
  realtimeCallKey,
  type RealtimeToolSurface,
} from './realtimeVoiceToolBridge';
import { useSessionStore } from '../store/sessionStore';

function toolDef(name: string): ToolDef {
  return {
    type: 'function',
    function: { name, description: name, parameters: { type: 'object', properties: {} } },
  };
}

const tool = toolDef('read_file');

const surface: RealtimeToolSurface = {
  tools: [tool, toolDef('write_file'), toolDef('mcp__notes__append')],
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
    // The bridge now digests the call key before it signals, so the flag flips
    // a couple of microtasks in rather than on the first one.
    await vi.waitFor(() => expect(waiting).toEqual([true]));
    release();
    await pending;
    expect(waiting).toEqual([true, false]);
  });
});

function issue(
  call: RealtimeVoiceToolCall,
  options: { execute: unknown; realtimeSessionId?: string; voiceTurnId?: string },
): Promise<string> {
  return executeRealtimeToolCall(call, {
    chatSessionId: 'chat',
    realtimeSessionId: options.realtimeSessionId ?? 'rv-1',
    surface,
    voiceTurnId: options.voiceTurnId,
    execute: options.execute as never,
  });
}

/** A dispatched call whose outcome was never recorded, exactly as the bridge
 * would have left the transcript if the process died between dispatch and
 * result: the tool_call row is there, the tool_result row is not. */
async function persistStartedToolCall(
  name: string,
  args: string,
  voiceTurnId: string,
  itemId: string,
): Promise<void> {
  useSessionStore.getState().addMessage('chat', {
    role: 'assistant',
    content: '',
    tool_calls: [{ id: `call-${itemId}`, type: 'function', function: { name, arguments: args } }],
    realtime: {
      sessionId: 'rv-1',
      itemId,
      turnId: `realtime:rv-1:${itemId}`,
      voiceTurnId,
      callKey: await realtimeCallKey(name, args),
      kind: 'tool_call',
    },
  });
}

/** A reconnect opens a brand-new provider conversation, so every item id in it
 * is new while the operation the model asks for is one the host may already
 * have performed. These pin the host-side identity that spans that gap, and —
 * just as importantly — the cases it must not swallow. */
describe('realtime tool identity across a reconnect', () => {
  it.each(['read_file', 'write_file'])(
    'BLOCKER 2 REGRESSION: a reconnect never executes %s twice',
    async (name) => {
      const execute = vi.fn(async () => '{"ok":true}');
      const args = '{"path":"notes.md","content":"one"}';
      const first = await issue(
        { id: 'call_A', itemId: 'item_A', name, arguments: args },
        { realtimeSessionId: 'rv-1', voiceTurnId: 'vt-1', execute },
      );
      const second = await issue(
        { id: 'call_B', itemId: 'item_B', name, arguments: args },
        { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute },
      );
      expect(execute).toHaveBeenCalledTimes(1);
      expect(second).toBe(first);
      // Answered straight from the transcript, so the reissue leaves no second
      // pair of rows that a later reader would score as two executions.
      expect(useSessionStore.getState().sessions[0].messages).toHaveLength(2);
    },
  );

  it('dedupes a reissue whose argument keys were re-serialized in another order', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    await issue(
      { id: 'call_A', itemId: 'item_A', name: 'write_file', arguments: '{"a":1,"b":2}' },
      { voiceTurnId: 'vt-1', execute },
    );
    await issue(
      { id: 'call_B', itemId: 'item_B', name: 'write_file', arguments: '{"b":2,"a":1}' },
      { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute },
    );
    expect(execute).toHaveBeenCalledTimes(1);
  });

  it('treats different argument values as a different operation and runs it', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    await issue(
      { id: 'call_A', itemId: 'item_A', name: 'write_file', arguments: '{"a":1,"b":2}' },
      { voiceTurnId: 'vt-1', execute },
    );
    await issue(
      { id: 'call_B', itemId: 'item_B', name: 'write_file', arguments: '{"a":1,"b":3}' },
      { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute },
    );
    expect(execute).toHaveBeenCalledTimes(2);
  });

  it('runs the same operation again when the user asks for it in a later voice turn', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    const args = '{"path":"notes.md","content":"one"}';
    await issue(
      { id: 'call_A', itemId: 'item_A', name: 'write_file', arguments: args },
      { voiceTurnId: 'vt-1', execute },
    );
    await issue(
      { id: 'call_B', itemId: 'item_B', name: 'write_file', arguments: args },
      { voiceTurnId: 'vt-2', execute },
    );
    expect(execute).toHaveBeenCalledTimes(2);
  });

  it('falls back to provider-item dedupe alone when no voice turn id is supplied', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    const call = { id: 'call_A', itemId: 'item_A', name: 'write_file', arguments: '{"path":"a"}' };
    const first = await issue(call, { execute });
    expect(await issue(call, { execute })).toBe(first);
    expect(execute).toHaveBeenCalledTimes(1);
    await issue(
      { ...call, id: 'call_B', itemId: 'item_B' },
      { realtimeSessionId: 'rv-2', execute },
    );
    expect(execute).toHaveBeenCalledTimes(2);
  });

  it.each(['write_file', 'mcp__notes__append'])(
    'refuses %s when it was already dispatched in this turn and its outcome is unknown',
    async (name) => {
      const execute = vi.fn(async () => '{"ok":true}');
      const args = '{"path":"a"}';
      await persistStartedToolCall(name, args, 'vt-1', 'item_A');
      const result = await issue(
        { id: 'call_B', itemId: 'item_B', name, arguments: args },
        { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute },
      );
      expect(execute).not.toHaveBeenCalled();
      expect(String(JSON.parse(result).error)).toContain('outcome is unknown');
    },
  );

  it('refuses even a read-only tool of unknown outcome, because the classification cannot be trusted', async () => {
    // Deciding this per tool would mean betting a side effect on a read-only
    // marking the frontend does not actually have: `isBlockedInPlanMode` misses
    // `device_action`, for one. The cost of failing closed is a single refusal
    // the operator clears by asking again, which starts a fresh voice turn.
    const execute = vi.fn(async () => '{"ok":true}');
    await persistStartedToolCall('read_file', '{"path":"a"}', 'vt-1', 'item_A');
    const result = await issue(
      { id: 'call_B', itemId: 'item_B', name: 'read_file', arguments: '{"path":"a"}' },
      { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute },
    );
    expect(execute).not.toHaveBeenCalled();
    expect(String(JSON.parse(result).error)).toContain('outcome is unknown');
  });

  it('runs a repeat of the same operation inside one provider session, because it is not a reissue', async () => {
    // The model reads a file it has just written, or re-runs a check, within a
    // single spoken turn. Only a reconnect — a new provider session id — may be
    // answered from the stored result; suppressing this would break ordinary
    // multi-step work in the name of reconnect safety.
    const execute = vi.fn(async () => '{"ok":true}');
    const call = { name: 'read_file', arguments: '{"path":"a"}' };
    await issue({ id: 'call_A', itemId: 'item_A', ...call }, { realtimeSessionId: 'rv-1', voiceTurnId: 'vt-1', execute });
    await issue({ id: 'call_B', itemId: 'item_B', ...call }, { realtimeSessionId: 'rv-1', voiceTurnId: 'vt-1', execute });
    expect(execute).toHaveBeenCalledTimes(2);
  });

  it('answers a reissue from an earlier session even after a same-session repeat', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    const call = { name: 'write_file', arguments: '{"path":"a"}' };
    await issue({ id: 'call_A', itemId: 'item_A', ...call }, { realtimeSessionId: 'rv-1', voiceTurnId: 'vt-1', execute });
    await issue({ id: 'call_C', itemId: 'item_C', ...call }, { realtimeSessionId: 'rv-2', voiceTurnId: 'vt-1', execute });
    expect(execute).toHaveBeenCalledTimes(1);
  });

  it('keeps the host-side identity out of every provider request body', async () => {
    const execute = vi.fn(async () => '{"ok":true}');
    await issue(
      { id: 'call_A', itemId: 'item_A', name: 'write_file', arguments: '{"path":"a"}' },
      { voiceTurnId: 'vt-1', execute },
    );
    const messages = useSessionStore.getState().sessions[0].messages;
    const callKey = messages[0].realtime?.callKey;
    expect(messages.map((message) => message.realtime?.voiceTurnId)).toEqual(['vt-1', 'vt-1']);
    expect(callKey).toMatch(/^[0-9a-f]{64}$/);
    const wire = JSON.stringify(toWireMessages(messages));
    expect(toWireMessages(messages).every((message) => !('realtime' in message))).toBe(true);
    expect(wire).not.toContain('vt-1');
    expect(wire).not.toContain(callKey);
  });
});

describe('realtimeCallKey', () => {
  it('is a stable hex digest that ignores argument key order but not the tool name', async () => {
    const key = await realtimeCallKey('write_file', '{"a":1,"b":{"d":4,"c":3}}');
    expect(key).toMatch(/^[0-9a-f]{64}$/);
    expect(await realtimeCallKey('write_file', '{"b":{"c":3,"d":4},"a":1}')).toBe(key);
    expect(await realtimeCallKey('read_file', '{"a":1,"b":{"d":4,"c":3}}')).not.toBe(key);
  });

  it('treats an absent argument object and an empty one as the same operation', async () => {
    // The normalizer emits `String(item.arguments ?? '')`, so the same
    // argument-less call can reach the bridge as '' in one session and '{}' in
    // the next — and then the reconnect dedupe would miss it.
    expect(await realtimeCallKey('computer_screenshot', '')).toBe(await realtimeCallKey('computer_screenshot', '{}'));
    expect(await realtimeCallKey('computer_screenshot', '   ')).toBe(await realtimeCallKey('computer_screenshot', '{}'));
  });

  it('keys unparseable arguments verbatim instead of throwing', async () => {
    const key = await realtimeCallKey('write_file', '{"path":');
    expect(key).toMatch(/^[0-9a-f]{64}$/);
    expect(await realtimeCallKey('write_file', '{"path":')).toBe(key);
    expect(await realtimeCallKey('write_file', '{"path"')).not.toBe(key);
  });
});

describe('appendRealtimeTranscript', () => {
  it('stamps the voice turn id only when one is supplied', () => {
    expect(appendRealtimeTranscript('chat', 'rv-1', 'a1', 'e1', 'assistant', 'stamped', 'vt-1')).toBe(true);
    expect(appendRealtimeTranscript('chat', 'rv-1', 'a2', 'e2', 'assistant', 'unstamped')).toBe(true);
    const [stamped, unstamped] = useSessionStore.getState().sessions[0].messages;
    expect(stamped.realtime?.voiceTurnId).toBe('vt-1');
    expect(unstamped.realtime && 'voiceTurnId' in unstamped.realtime).toBe(false);
  });
});
