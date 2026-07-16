import { describe, expect, it } from 'vitest';

import {
  classifyExternalTool,
  classifyTurnToolCalls,
  isMcpToolName,
  needsReconciliation,
  turnMessageRange,
  type ExternalEffect,
} from './checkpointReconciliation';
import type { ChatMessage, ToolCall } from './llamaClient';

function userMsg(text: string): ChatMessage {
  return { role: 'user', content: text };
}

function assistantMsg(text: string, toolCalls?: ToolCall[]): ChatMessage {
  return { role: 'assistant', content: text, tool_calls: toolCalls };
}

function toolMsg(toolCallId: string, content: string): ChatMessage {
  return { role: 'tool', content, tool_call_id: toolCallId };
}

let nextCallId = 0;
function call(name: string, args: Record<string, unknown> = {}): ToolCall {
  nextCallId += 1;
  return { id: `call-${nextCallId}`, type: 'function', function: { name, arguments: JSON.stringify(args) } };
}

describe('turnMessageRange', () => {
  it('spans from the anchor to the next user message', () => {
    const messages = [userMsg('turn 1'), assistantMsg('reply 1'), userMsg('turn 2'), assistantMsg('reply 2')];
    expect(turnMessageRange(messages, 0)).toEqual([0, 2]);
  });

  it('spans to the end of the transcript for the most recent turn', () => {
    const messages = [userMsg('turn 1'), assistantMsg('reply 1'), assistantMsg('reply 1b')];
    expect(turnMessageRange(messages, 0)).toEqual([0, 3]);
  });

  it('returns an empty range for an out-of-bounds anchor', () => {
    const messages = [userMsg('only message')];
    expect(turnMessageRange(messages, 5)).toEqual([5, 5]);
    expect(turnMessageRange(messages, -1)).toEqual([-1, -1]);
  });

  it('handles multiple turns, isolating each one correctly', () => {
    const messages = [
      userMsg('turn 1'),
      assistantMsg('reply 1'),
      userMsg('turn 2'),
      assistantMsg('reply 2a'),
      assistantMsg('reply 2b'),
      userMsg('turn 3'),
      assistantMsg('reply 3'),
    ];
    expect(turnMessageRange(messages, 2)).toEqual([2, 5]);
    expect(turnMessageRange(messages, 5)).toEqual([5, 7]);
  });
});

describe('classifyExternalTool / isMcpToolName', () => {
  it('classifies run_shell as shell', () => {
    expect(classifyExternalTool('run_shell')).toBe('shell');
  });

  it('classifies web_fetch and web_search as network', () => {
    expect(classifyExternalTool('web_fetch')).toBe('network');
    expect(classifyExternalTool('web_search')).toBe('network');
  });

  it('classifies remember as memory', () => {
    expect(classifyExternalTool('remember')).toBe('memory');
  });

  it('classifies any mcp__-prefixed tool as mcp', () => {
    expect(isMcpToolName('mcp__github__create_issue')).toBe(true);
    expect(classifyExternalTool('mcp__github__create_issue')).toBe('mcp');
    expect(classifyExternalTool('mcp__stripe__create_charge')).toBe('mcp');
  });

  it('returns null for file tools and pure-read tools', () => {
    expect(classifyExternalTool('write_file')).toBeNull();
    expect(classifyExternalTool('edit_file')).toBeNull();
    expect(classifyExternalTool('read_file')).toBeNull();
    expect(classifyExternalTool('grep')).toBeNull();
    expect(classifyExternalTool('list_dir')).toBeNull();
    expect(classifyExternalTool('present_plan')).toBeNull();
  });
});

describe('classifyTurnToolCalls', () => {
  it('buckets a turn that only wrote files as fileTools with no external effects', () => {
    const messages = [
      userMsg('fix the bug'),
      assistantMsg('working on it', [call('read_file', { path: 'a.ts' })]),
      toolMsg('call-1', 'file contents'),
      assistantMsg('done', [call('write_file', { path: 'a.ts', content: 'x' })]),
      toolMsg('call-2', 'wrote a.ts'),
    ];
    const effects = classifyTurnToolCalls(messages, 0);
    expect(effects.fileTools).toEqual(['write_file']);
    expect(effects.external).toEqual([]);
  });

  it('flags a shell command as an external effect', () => {
    const messages = [
      userMsg('run the tests'),
      assistantMsg('running', [call('run_shell', { command: 'pnpm test' })]),
      toolMsg('call-1', 'tests passed'),
    ];
    const effects = classifyTurnToolCalls(messages, 0);
    expect(effects.external).toEqual([{ tool: 'run_shell', kind: 'shell' }]);
  });

  it('flags a network call and an MCP tool call as distinct external effects', () => {
    const messages = [
      userMsg('look this up and file a ticket'),
      assistantMsg('searching', [call('web_search', { query: 'foo' })]),
      toolMsg('call-1', 'results'),
      assistantMsg('filing', [call('mcp__linear__create_issue', { title: 'foo' })]),
      toolMsg('call-2', 'created'),
    ];
    const effects = classifyTurnToolCalls(messages, 0);
    expect(effects.external).toEqual(
      expect.arrayContaining([
        { tool: 'web_search', kind: 'network' },
        { tool: 'mcp__linear__create_issue', kind: 'mcp' },
      ]),
    );
    expect(effects.external).toHaveLength(2);
  });

  it('deduplicates repeated calls to the same tool within one turn', () => {
    const messages = [
      userMsg('run several commands'),
      assistantMsg('running', [call('run_shell', { command: 'echo 1' }), call('run_shell', { command: 'echo 2' })]),
      toolMsg('call-1', 'ok'),
      toolMsg('call-2', 'ok'),
    ];
    const effects = classifyTurnToolCalls(messages, 0);
    expect(effects.external).toEqual([{ tool: 'run_shell', kind: 'shell' }]);
  });

  it('only scans tool calls within this turn, not earlier or later turns', () => {
    const messages = [
      userMsg('turn 1: run shell'),
      assistantMsg('running', [call('run_shell', { command: 'echo 1' })]),
      toolMsg('call-1', 'ok'),
      userMsg('turn 2: just write a file'),
      assistantMsg('writing', [call('write_file', { path: 'a.ts', content: 'x' })]),
      toolMsg('call-2', 'wrote'),
    ];
    const turn2Effects = classifyTurnToolCalls(messages, 3);
    expect(turn2Effects.fileTools).toEqual(['write_file']);
    expect(turn2Effects.external).toEqual([]);

    const turn1Effects = classifyTurnToolCalls(messages, 0);
    expect(turn1Effects.external).toEqual([{ tool: 'run_shell', kind: 'shell' }]);
  });

  it('returns empty buckets for a turn with no tool calls at all', () => {
    const messages = [userMsg('just chat'), assistantMsg('sure, here is an answer')];
    const effects = classifyTurnToolCalls(messages, 0);
    expect(effects.fileTools).toEqual([]);
    expect(effects.external).toEqual([]);
  });
});

describe('needsReconciliation', () => {
  it('is false when neither signal reports an external effect', () => {
    expect(needsReconciliation(false, [])).toBe(false);
  });

  it('is true when the backend-tracked shellRan flag is set, even with no transcript detail', () => {
    expect(needsReconciliation(true, [])).toBe(true);
  });

  it('is true when the transcript-derived external list is non-empty, even if shellRan is false', () => {
    const external: ExternalEffect[] = [{ tool: 'web_fetch', kind: 'network' }];
    expect(needsReconciliation(false, external)).toBe(true);
  });

  it('is true when both signals report an external effect', () => {
    const external: ExternalEffect[] = [{ tool: 'mcp__stripe__create_charge', kind: 'mcp' }];
    expect(needsReconciliation(true, external)).toBe(true);
  });
});
