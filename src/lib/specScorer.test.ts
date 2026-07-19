import { describe, expect, it, vi } from 'vitest';

import {
  isSpecTooVague,
  parseSpecScoreResponse,
  scoreSpec,
  SPEC_SCORE_WARN_THRESHOLD,
  SPEC_SCORER_TIMEOUT_MS,
  type SpecScorerCallResult,
} from './specScorer';
import type { ChatMessage } from './llamaClient';

const VALID_JSON =
  '{"dimensions":{"clarity":80,"scope":70,"missingContext":60,"testability":50,"dependencies":90,"agentReadiness":75},"missingInfo":["What is the expected input format?"],"summary":"Mostly clear, missing acceptance criteria."}';

describe('parseSpecScoreResponse', () => {
  it('parses a well-formed scorer reply and computes the deterministic overall average', () => {
    expect(parseSpecScoreResponse(VALID_JSON)).toEqual({
      overall: Math.round((80 + 70 + 60 + 50 + 90 + 75) / 6),
      dimensions: { clarity: 80, scope: 70, missingContext: 60, testability: 50, dependencies: 90, agentReadiness: 75 },
      missingInfo: ['What is the expected input format?'],
      summary: 'Mostly clear, missing acceptance criteria.',
    });
  });

  it('salvages a JSON object embedded in extra prose', () => {
    const result = parseSpecScoreResponse(`Sure, here you go:\n${VALID_JSON}\nHope that helps!`);
    expect(result?.overall).toBe(Math.round((80 + 70 + 60 + 50 + 90 + 75) / 6));
  });

  it('clamps out-of-range dimension scores into 0-100', () => {
    const json =
      '{"dimensions":{"clarity":150,"scope":-20,"missingContext":50,"testability":50,"dependencies":50,"agentReadiness":50},"missingInfo":[],"summary":"x"}';
    const result = parseSpecScoreResponse(json);
    expect(result?.dimensions.clarity).toBe(100);
    expect(result?.dimensions.scope).toBe(0);
  });

  it('defaults missingInfo to an empty array and caps it at 10 items', () => {
    const many = Array.from({ length: 15 }, (_, i) => `Question ${i}`);
    const json = JSON.stringify({
      dimensions: { clarity: 50, scope: 50, missingContext: 50, testability: 50, dependencies: 50, agentReadiness: 50 },
      missingInfo: many,
      summary: 'x',
    });
    expect(parseSpecScoreResponse(json)?.missingInfo).toHaveLength(10);

    const noMissingInfo = JSON.stringify({
      dimensions: { clarity: 50, scope: 50, missingContext: 50, testability: 50, dependencies: 50, agentReadiness: 50 },
      summary: 'x',
    });
    expect(parseSpecScoreResponse(noMissingInfo)?.missingInfo).toEqual([]);
  });

  it('filters out blank/non-string missingInfo entries', () => {
    const json = JSON.stringify({
      dimensions: { clarity: 50, scope: 50, missingContext: 50, testability: 50, dependencies: 50, agentReadiness: 50 },
      missingInfo: ['  real question  ', '', '   ', 42, null],
      summary: 'x',
    });
    expect(parseSpecScoreResponse(json)?.missingInfo).toEqual(['real question']);
  });

  it('defaults a missing/non-string summary to an empty string rather than failing the whole parse', () => {
    const json = JSON.stringify({
      dimensions: { clarity: 50, scope: 50, missingContext: 50, testability: 50, dependencies: 50, agentReadiness: 50 },
      missingInfo: [],
    });
    expect(parseSpecScoreResponse(json)?.summary).toBe('');
  });

  // Fail-closed: every one of these must return null, NEVER a fabricated score.
  it('fails closed on malformed JSON', () => {
    expect(parseSpecScoreResponse('not json at all')).toBeNull();
  });

  it('fails closed on a missing dimensions object', () => {
    expect(parseSpecScoreResponse('{"missingInfo":[],"summary":"x"}')).toBeNull();
  });

  it('fails closed when a dimension is missing or non-numeric', () => {
    const missingOne = JSON.stringify({
      dimensions: { clarity: 50, scope: 50, missingContext: 50, testability: 50, dependencies: 50 },
      missingInfo: [],
      summary: 'x',
    });
    expect(parseSpecScoreResponse(missingOne)).toBeNull();

    const nonNumeric = JSON.stringify({
      dimensions: { clarity: 'high', scope: 50, missingContext: 50, testability: 50, dependencies: 50, agentReadiness: 50 },
      missingInfo: [],
      summary: 'x',
    });
    expect(parseSpecScoreResponse(nonNumeric)).toBeNull();
  });

  it('fails closed on a non-object JSON value', () => {
    expect(parseSpecScoreResponse('"low"')).toBeNull();
    expect(parseSpecScoreResponse('42')).toBeNull();
    expect(parseSpecScoreResponse('null')).toBeNull();
  });
});

describe('isSpecTooVague', () => {
  it('is true strictly below the warn threshold', () => {
    expect(isSpecTooVague({ overall: SPEC_SCORE_WARN_THRESHOLD - 1, dimensions: {} as never, missingInfo: [], summary: '' })).toBe(true);
  });

  it('is false at or above the warn threshold', () => {
    expect(isSpecTooVague({ overall: SPEC_SCORE_WARN_THRESHOLD, dimensions: {} as never, missingInfo: [], summary: '' })).toBe(false);
    expect(isSpecTooVague({ overall: 100, dimensions: {} as never, missingInfo: [], summary: '' })).toBe(false);
  });
});

function okCallModel(content: string): (messages: ChatMessage[], signal: AbortSignal) => Promise<SpecScorerCallResult> {
  return async () => ({ content, streamError: null });
}

describe('scoreSpec', () => {
  it('returns the parsed score on a well-formed reply', async () => {
    const result = await scoreSpec('Fix the login bug', 'Steps to reproduce...', okCallModel(VALID_JSON));
    expect(result?.summary).toBe('Mostly clear, missing acceptance criteria.');
    expect(result?.missingInfo).toEqual(['What is the expected input format?']);
  });

  it('fails closed when callModel reports a stream error', async () => {
    const callModel = vi.fn(async (): Promise<SpecScorerCallResult> => ({ content: '', streamError: 'network error' }));
    const result = await scoreSpec('Title', 'Body', callModel);
    expect(result).toBeNull();
  });

  it('fails closed when callModel throws', async () => {
    const callModel = vi.fn(async (): Promise<SpecScorerCallResult> => {
      throw new Error('boom');
    });
    const result = await scoreSpec('Title', 'Body', callModel);
    expect(result).toBeNull();
  });

  it('fails closed when callModel returns unparseable content', async () => {
    const result = await scoreSpec('Title', 'Body', okCallModel('not json'));
    expect(result).toBeNull();
  });

  it('times out and fails closed after SPEC_SCORER_TIMEOUT_MS, never hanging the caller', async () => {
    vi.useFakeTimers();
    try {
      let sawAbort = false;
      const callModel = (_messages: ChatMessage[], signal: AbortSignal): Promise<SpecScorerCallResult> => {
        return new Promise((_resolve, reject) => {
          signal.addEventListener('abort', () => {
            sawAbort = true;
            reject(new Error('aborted'));
          });
        });
      };

      const promise = scoreSpec('Title', 'Body', callModel);
      await vi.advanceTimersByTimeAsync(SPEC_SCORER_TIMEOUT_MS + 10);

      const result = await promise;
      expect(result).toBeNull();
      expect(sawAbort).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it('aborts the scoring call when the caller-supplied signal aborts', async () => {
    const controller = new AbortController();
    let sawAbort = false;
    const callModel = (_messages: ChatMessage[], signal: AbortSignal): Promise<SpecScorerCallResult> => {
      return new Promise((_resolve, reject) => {
        signal.addEventListener('abort', () => {
          sawAbort = true;
          reject(new Error('aborted'));
        });
      });
    };

    const promise = scoreSpec('Title', 'Body', callModel, controller.signal);
    controller.abort();

    const result = await promise;
    expect(result).toBeNull();
    expect(sawAbort).toBe(true);
  });

  it('passes the issue title and body through to the rubric prompt', async () => {
    const callModel = vi.fn(okCallModel(VALID_JSON));
    await scoreSpec('My Issue Title', 'My issue body detail', callModel);

    expect(callModel).toHaveBeenCalledTimes(1);
    const [messages] = callModel.mock.calls[0] as [ChatMessage[], AbortSignal];
    const userMessage = messages.find((m) => m.role === 'user');
    expect(String(userMessage?.content)).toContain('My Issue Title');
    expect(String(userMessage?.content)).toContain('My issue body detail');
  });
});
