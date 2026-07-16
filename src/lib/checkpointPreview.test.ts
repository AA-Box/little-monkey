import { describe, expect, it } from 'vitest';

import { formatVerifyNotice } from './agentLoop';
import { gatherTurnContext } from './checkpointPreview';
import type { ChatMessage } from './llamaClient';

function userMsg(text: string): ChatMessage {
  return { role: 'user', content: text };
}

function userMsgWithImage(text: string, url: string): ChatMessage {
  return {
    role: 'user',
    content: [
      { type: 'text', text },
      { type: 'image_url', image_url: { url } },
    ],
  };
}

function assistantMsg(text: string): ChatMessage {
  return { role: 'assistant', content: text };
}

function assistantMsgWithShell(): ChatMessage {
  return {
    role: 'assistant',
    content: 'running a command',
    tool_calls: [{ id: 'c1', type: 'function', function: { name: 'run_shell', arguments: '{}' } }],
  };
}

function verifySystemMsg(): ChatMessage {
  return {
    role: 'system',
    content: formatVerifyNotice({ label: 'pnpm test', kind: 'test', ok: true, code: 0, output: 'all green', durationMs: 1200 }),
  };
}

describe('gatherTurnContext', () => {
  it('extracts an artifact fence produced within the turn', () => {
    const messages = [
      userMsg('build me a widget'),
      assistantMsg('Here you go:\n\n```html\n<div>hi</div>\n```\n'),
    ];
    const context = gatherTurnContext(messages, 0, 'build me a widget', false);
    expect(context.artifacts).toHaveLength(1);
    expect(context.artifacts[0].kind).toBe('html');
  });

  it('does not pull in an artifact from a different turn', () => {
    const messages = [
      userMsg('turn 1'),
      assistantMsg('```html\n<div>turn1</div>\n```'),
      userMsg('turn 2'),
      assistantMsg('just chatting, no fences here'),
    ];
    const turn2Context = gatherTurnContext(messages, 2, 'turn 2', false);
    expect(turn2Context.artifacts).toHaveLength(0);

    const turn1Context = gatherTurnContext(messages, 0, 'turn 1', false);
    expect(turn1Context.artifacts).toHaveLength(1);
  });

  it('collects image attachments within the turn', () => {
    const messages = [userMsgWithImage('see this screenshot', 'data:image/png;base64,abc'), assistantMsg('got it')];
    const context = gatherTurnContext(messages, 0, 'see this screenshot', false);
    expect(context.images).toEqual([{ messageIndex: 0, url: 'data:image/png;base64,abc' }]);
  });

  it('collects verify notices reported within the turn', () => {
    const messages = [userMsg('fix and verify'), assistantMsg('fixed it'), verifySystemMsg()];
    const context = gatherTurnContext(messages, 0, 'fix and verify', false);
    expect(context.verify).toHaveLength(1);
    expect(context.verify[0]).toMatchObject({ label: 'pnpm test', ok: true });
  });

  it('flags needsReconciliation when the turn ran a shell command, mirroring shellRan', () => {
    const messages = [userMsg('run tests'), assistantMsgWithShell()];
    const context = gatherTurnContext(messages, 0, 'run tests', false);
    expect(context.external).toEqual([{ tool: 'run_shell', kind: 'shell' }]);
    expect(context.needsReconciliation).toBe(true);
  });

  it('flags needsReconciliation from the backend shellRan flag even with an empty transcript slice', () => {
    const messages = [userMsg('turn with compacted-away tool calls')];
    const context = gatherTurnContext(messages, 0, 'turn with compacted-away tool calls', true);
    expect(context.external).toEqual([]);
    expect(context.needsReconciliation).toBe(true);
  });

  it('reports conversationRewindAvailable false when the anchor no longer matches', () => {
    const messages = [userMsg('this text has changed since the checkpoint was recorded')];
    const context = gatherTurnContext(messages, 0, 'original prompt label', false);
    expect(context.conversationRewindAvailable).toBe(false);
  });

  it('reports conversationRewindAvailable true when the anchor still matches', () => {
    const messages = [userMsg('original prompt label and then some more text'), assistantMsg('ok')];
    const context = gatherTurnContext(messages, 0, 'original prompt label', false);
    expect(context.conversationRewindAvailable).toBe(true);
  });
});
