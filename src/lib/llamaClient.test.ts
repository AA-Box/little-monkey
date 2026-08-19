import { describe, expect, it, vi } from 'vitest';

import { SseEventParser, streamChat, textContent, type StreamEvent } from './llamaClient';

function collect(parser: SseEventParser, chunks: string[]): StreamEvent[] {
  const events: StreamEvent[] = [];
  for (const chunk of chunks) {
    for (const event of parser.feed(chunk)) events.push(event);
  }
  for (const event of parser.flush()) events.push(event);
  return events;
}

function dataLine(payload: object): string {
  return `data: ${JSON.stringify(payload)}\n`;
}

describe('textContent', () => {
  it('passes plain strings through', () => {
    expect(textContent('hello')).toBe('hello');
  });

  it('joins only the text parts of multi-part content', () => {
    expect(
      textContent([
        { type: 'text', text: 'a' },
        { type: 'image_url', image_url: { url: 'data:...' } },
        { type: 'text', text: 'b' },
      ])
    ).toBe('a\nb');
  });
});

describe('SseEventParser', () => {
  it('yields content deltas', () => {
    const events = collect(new SseEventParser(), [
      dataLine({ choices: [{ delta: { content: 'Hel' } }] }),
      dataLine({ choices: [{ delta: { content: 'lo' } }] }),
      'data: [DONE]\n',
    ]);
    expect(events).toEqual([
      { type: 'delta', content: 'Hel' },
      { type: 'delta', content: 'lo' },
    ]);
  });

  it('reassembles a line split across chunk boundaries', () => {
    const line = dataLine({ choices: [{ delta: { content: 'split' } }] });
    const events = collect(new SseEventParser(), [line.slice(0, 12), line.slice(12)]);
    expect(events).toEqual([{ type: 'delta', content: 'split' }]);
  });

  it('accumulates streamed tool-call fragments until finish_reason', () => {
    const events = collect(new SseEventParser(), [
      dataLine({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_1', function: { name: 'grep', arguments: '{"pat' } }] } }] }),
      dataLine({ choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: 'tern":"x"}' } }] } }] }),
      dataLine({ choices: [{ delta: {}, finish_reason: 'tool_calls' }] }),
    ]);
    expect(events).toEqual([
      {
        type: 'tool_call',
        toolCall: { id: 'call_1', type: 'function', function: { name: 'grep', arguments: '{"pattern":"x"}' } },
      },
    ]);
  });

  it('flushes a still-pending tool call when the stream ends without finish_reason', () => {
    const events = collect(new SseEventParser(), [
      dataLine({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_9', function: { name: 'read_file', arguments: '{}' } }] } }] }),
    ]);
    expect(events).toEqual([
      {
        type: 'tool_call',
        toolCall: { id: 'call_9', type: 'function', function: { name: 'read_file', arguments: '{}' } },
      },
    ]);
  });

  it('yields usage from the final include_usage chunk', () => {
    const events = collect(new SseEventParser(), [
      dataLine({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } }),
    ]);
    expect(events).toEqual([
      { type: 'usage', usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
    ]);
  });

  it('skips malformed payloads without crashing', () => {
    const events = collect(new SseEventParser(), ['data: {not json}\n', dataLine({ choices: [{ delta: { content: 'ok' } }] })]);
    expect(events).toEqual([{ type: 'delta', content: 'ok' }]);
  });
});

describe('streamChat request shape', () => {
  it('preserves image_url content for a vision-capable local request', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('data: [DONE]\n', { status: 200, headers: { 'Content-Type': 'text/event-stream' } }),
    );
    const messages = [{
      role: 'user' as const,
      content: [
        { type: 'text' as const, text: 'What is this?' },
        { type: 'image_url' as const, image_url: { url: 'data:image/png;base64,AAAA' } },
      ],
    }];

    for await (const _event of streamChat('http://127.0.0.1:8080', messages, [], 'model')) {
      // Drain the stream.
    }

    const init = fetchMock.mock.calls[0][1] as RequestInit;
    const body = JSON.parse(init.body as string) as { messages: typeof messages };
    expect(body.messages).toEqual(messages);
    fetchMock.mockRestore();
  });

  it('omits tools and tool_choice entirely for a no-tools request', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('data: [DONE]\n', { status: 200, headers: { 'Content-Type': 'text/event-stream' } })
    );

    for await (const _event of streamChat('http://127.0.0.1:11434', [], [], 'model')) {
      // Drain the stream.
    }

    const init = fetchMock.mock.calls[0][1] as RequestInit;
    const body = JSON.parse(init.body as string) as Record<string, unknown>;
    expect(body).not.toHaveProperty('tools');
    expect(body).not.toHaveProperty('tool_choice');
    fetchMock.mockRestore();
  });

  it('includes auto tool choice when at least one tool is offered', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('data: [DONE]\n', { status: 200, headers: { 'Content-Type': 'text/event-stream' } })
    );
    const tools = [{ type: 'function' as const, function: { name: 'read_file', description: 'Read', parameters: {} } }];

    for await (const _event of streamChat('http://127.0.0.1:11434', [], tools, 'model')) {
      // Drain the stream.
    }

    const init = fetchMock.mock.calls[0][1] as RequestInit;
    const body = JSON.parse(init.body as string) as Record<string, unknown>;
    expect(body.tools).toEqual(tools);
    expect(body.tool_choice).toBe('auto');
    fetchMock.mockRestore();
  });

  it('sends an explicit max_tokens ceiling when a bounded caller supplies one', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('data: [DONE]\n', { status: 200, headers: { 'Content-Type': 'text/event-stream' } })
    );

    for await (const _event of streamChat('http://127.0.0.1:11434', [], [], 'model', undefined, 2048)) {
      // Drain the stream.
    }

    const init = fetchMock.mock.calls[0][1] as RequestInit;
    const body = JSON.parse(init.body as string) as Record<string, unknown>;
    expect(body.max_tokens).toBe(2048);
    fetchMock.mockRestore();
  });
});
