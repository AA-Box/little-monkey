import { describe, expect, it, vi } from 'vitest';

import {
  recoverTextToolCalls,
  SseEventParser,
  streamChat,
  textContent,
  type StreamEvent,
  type ToolDef,
} from './llamaClient';

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

  it('throws the message from an error frame instead of streaming nothing', () => {
    // What `mlx_chat.rs` sends when the runtime cannot load the model: a 200
    // response whose only frame is an error. Swallowing it leaves an empty
    // assistant bubble and no explanation anywhere in the UI.
    const parser = new SseEventParser();
    expect(() =>
      collect(parser, [
        'event: error\n',
        dataLine({ error: { message: 'process exited before readiness: signal 6', type: 'little_monkey_m3_error' } }),
      ])
    ).toThrow('process exited before readiness: signal 6');
  });

  it('throws a bare-string error frame, the shape other OpenAI-compatible servers send', () => {
    // LM Studio and Ollama-style `/v1` proxies report `{"error": "..."}`;
    // reading only `error.message` there would drop the one thing this branch
    // exists to surface.
    expect(() =>
      collect(new SseEventParser(), [dataLine({ error: 'context length exceeded' })])
    ).toThrow('context length exceeded');
  });

  it('still reports an error frame that carries no message', () => {
    expect(() => collect(new SseEventParser(), [dataLine({ error: {} })])).toThrow(
      /without saying why/
    );
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

  it('rejects with the error frame of an otherwise-successful stream', async () => {
    // The mid-stream catch in `streamChat` only swallows an aborted fetch, so
    // an error frame on a live stream has to come back out to the caller.
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        'data: {"error":{"type":"api_error","code":"model_not_found","message":"mlx-vlm cannot load qwen3_5"}}\n',
        { status: 200, headers: { 'Content-Type': 'text/event-stream' } }
      )
    );

    try {
      await expect(async () => {
        for await (const _event of streamChat('http://127.0.0.1:8081', [], [], 'model')) {
          // Drain the stream.
        }
      }).rejects.toThrow('mlx-vlm cannot load qwen3_5');
    } finally {
      // Restored even on failure: a leaked fetch spy makes the *next* test in
      // this describe read this test's call as its own.
      fetchMock.mockRestore();
    }
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

describe('recoverTextToolCalls', () => {
  const tools: ToolDef[] = [
    { type: 'function', function: { name: 'run_shell', description: '', parameters: {} } },
    { type: 'function', function: { name: 'edit_file', description: '', parameters: {} } },
  ];

  it('recovers a fenced tool call the model wrote as prose and strips it from the answer', () => {
    const recovered = recoverTextToolCalls(
      'Let me check the log.\n\n```json\n{\n  "name": "run_shell",\n  "arguments": {\n    "command": "cat ~/Library/Logs/bf6.log"\n  }\n}\n```\n',
      tools,
    );
    expect(recovered.toolCalls).toEqual([
      {
        id: 'call_text_0',
        type: 'function',
        function: { name: 'run_shell', arguments: '{"command":"cat ~/Library/Logs/bf6.log"}' },
      },
    ]);
    expect(recovered.content).toBe('Let me check the log.');
  });

  it('collapses a call the model restated twice in one message', () => {
    const block = '```json\n{"name": "run_shell", "arguments": {"command": "ls"}}\n```';
    const recovered = recoverTextToolCalls(`Do it.\n\n${block}\n\nRunning it now.\n${block}`, tools);
    expect(recovered.toolCalls).toHaveLength(1);
    expect(recovered.content).toBe('Do it.\n\nRunning it now.');
  });

  it('recovers a Hermes-style tool_call tag the server template did not parse', () => {
    const recovered = recoverTextToolCalls(
      '<tool_call>{"name": "edit_file", "arguments": {"path": "a.ts", "old_string": "}", "new_string": "{"}}</tool_call>',
      tools,
    );
    expect(recovered.toolCalls[0]?.function).toEqual({
      name: 'edit_file',
      arguments: '{"path":"a.ts","old_string":"}","new_string":"{"}',
    });
    expect(recovered.content).toBe('');
  });

  it('leaves JSON that is not a call for an offered tool completely alone', () => {
    const prose = 'The config is `{"name": "little-monkey", "arguments": {"command": "x"}}` — note the name.';
    expect(recoverTextToolCalls(prose, tools)).toEqual({ content: prose, toolCalls: [] });

    const documented = '{"name": "run_shell", "arguments": {"command": "ls"}, "note": "example only"}';
    expect(recoverTextToolCalls(documented, tools)).toEqual({ content: documented, toolCalls: [] });

    const noArgs = '{"name": "run_shell"}';
    expect(recoverTextToolCalls(noArgs, tools)).toEqual({ content: noArgs, toolCalls: [] });
  });

  it('recovers nothing when no tools were offered this turn', () => {
    const content = '{"name": "run_shell", "arguments": {"command": "ls"}}';
    expect(recoverTextToolCalls(content, [])).toEqual({ content, toolCalls: [] });
  });
});
