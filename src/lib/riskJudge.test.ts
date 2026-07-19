import { describe, expect, it, vi } from 'vitest';

import { classifyToolCall, JUDGE_TIMEOUT_MS, parseJudgeResponse, riskCacheKey, type JudgeCallResult } from './riskJudge';
import type { ChatMessage } from './llamaClient';

describe('parseJudgeResponse', () => {
  it('parses a well-formed judge reply', () => {
    expect(parseJudgeResponse('{"level":"medium","reason":"touches auth logic"}')).toEqual({
      level: 'medium',
      reason: 'touches auth logic',
    });
  });

  it('trims whitespace around the reason', () => {
    expect(parseJudgeResponse('{"level":"low","reason":"  routine change  "}')).toEqual({
      level: 'low',
      reason: 'routine change',
    });
  });

  it('salvages a JSON object embedded in extra prose', () => {
    expect(parseJudgeResponse('Sure, here you go:\n{"level":"high","reason":"deletes files"}\nHope that helps!')).toEqual({
      level: 'high',
      reason: 'deletes files',
    });
  });

  // Fail-closed: every one of these must return null, NEVER a fabricated
  // classification and NEVER silently default to "low".
  it('fails closed on malformed JSON', () => {
    expect(parseJudgeResponse('not json at all')).toBeNull();
  });

  it('fails closed on an out-of-enum level', () => {
    expect(parseJudgeResponse('{"level":"critical","reason":"whatever"}')).toBeNull();
  });

  it('fails closed on a missing level', () => {
    expect(parseJudgeResponse('{"reason":"whatever"}')).toBeNull();
  });

  it('fails closed on a missing or blank reason', () => {
    expect(parseJudgeResponse('{"level":"low"}')).toBeNull();
    expect(parseJudgeResponse('{"level":"low","reason":""}')).toBeNull();
    expect(parseJudgeResponse('{"level":"low","reason":"   "}')).toBeNull();
  });

  it('fails closed on a non-object JSON value', () => {
    expect(parseJudgeResponse('"low"')).toBeNull();
    expect(parseJudgeResponse('42')).toBeNull();
    expect(parseJudgeResponse('null')).toBeNull();
  });
});

describe('riskCacheKey', () => {
  it('is identical for identical (tool, args) pairs', () => {
    expect(riskCacheKey('write_file', { path: 'a.txt', content: 'hi' })).toBe(
      riskCacheKey('write_file', { path: 'a.txt', content: 'hi' })
    );
  });

  it('differs when the tool or args differ', () => {
    const base = riskCacheKey('write_file', { path: 'a.txt' });
    expect(riskCacheKey('edit_file', { path: 'a.txt' })).not.toBe(base);
    expect(riskCacheKey('write_file', { path: 'b.txt' })).not.toBe(base);
  });
});

function okCallModel(content: string): (messages: ChatMessage[], signal: AbortSignal) => Promise<JudgeCallResult> {
  return async () => ({ content, streamError: null });
}

describe('classifyToolCall', () => {
  it('returns the parsed classification on a well-formed reply', async () => {
    const result = await classifyToolCall(
      'write_file',
      { path: 'src/index.ts', content: 'x' },
      '/workspace',
      okCallModel('{"level":"low","reason":"routine source edit"}')
    );
    expect(result).toEqual({ level: 'low', reason: 'routine source edit' });
  });

  it('fails closed when callModel reports a stream error', async () => {
    const callModel = vi.fn(async (): Promise<JudgeCallResult> => ({ content: '', streamError: 'network error' }));
    const result = await classifyToolCall('run_shell', { command: 'ls' }, '/workspace', callModel);
    expect(result).toBeNull();
  });

  it('fails closed when callModel throws', async () => {
    const callModel = vi.fn(async (): Promise<JudgeCallResult> => {
      throw new Error('boom');
    });
    const result = await classifyToolCall('run_shell', { command: 'ls' }, '/workspace', callModel);
    expect(result).toBeNull();
  });

  it('fails closed when callModel returns unparseable content', async () => {
    const result = await classifyToolCall('edit_file', { path: 'a.txt' }, '/workspace', okCallModel('not json'));
    expect(result).toBeNull();
  });

  it('times out and fails closed after JUDGE_TIMEOUT_MS, never hanging the caller', async () => {
    vi.useFakeTimers();
    try {
      let sawAbort = false;
      const callModel = (_messages: ChatMessage[], signal: AbortSignal): Promise<JudgeCallResult> => {
        // Never resolves on its own — only the timeout-driven abort ends this.
        return new Promise((_resolve, reject) => {
          signal.addEventListener('abort', () => {
            sawAbort = true;
            reject(new Error('aborted'));
          });
        });
      };

      const promise = classifyToolCall('write_file', { path: 'a.txt' }, '/workspace', callModel);
      await vi.advanceTimersByTimeAsync(JUDGE_TIMEOUT_MS + 10);

      const result = await promise;
      expect(result).toBeNull();
      expect(sawAbort).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it('aborts the judge call when the caller-supplied signal aborts', async () => {
    const controller = new AbortController();
    let sawAbort = false;
    const callModel = (_messages: ChatMessage[], signal: AbortSignal): Promise<JudgeCallResult> => {
      return new Promise((_resolve, reject) => {
        signal.addEventListener('abort', () => {
          sawAbort = true;
          reject(new Error('aborted'));
        });
      });
    };

    const promise = classifyToolCall('write_file', { path: 'a.txt' }, '/workspace', callModel, controller.signal);
    controller.abort();

    const result = await promise;
    expect(result).toBeNull();
    expect(sawAbort).toBe(true);
  });

  it('passes the workspace root and tool/args through to the judge prompt', async () => {
    const callModel = vi.fn(okCallModel('{"level":"high","reason":"env file"}'));
    await classifyToolCall('write_file', { path: '.env' }, '/my/workspace', callModel);

    expect(callModel).toHaveBeenCalledTimes(1);
    const [messages] = callModel.mock.calls[0] as [ChatMessage[], AbortSignal];
    const userMessage = messages.find((m) => m.role === 'user');
    expect(userMessage).toBeDefined();
    expect(String(userMessage?.content)).toContain('/my/workspace');
    expect(String(userMessage?.content)).toContain('write_file');
    expect(String(userMessage?.content)).toContain('.env');
  });
});
