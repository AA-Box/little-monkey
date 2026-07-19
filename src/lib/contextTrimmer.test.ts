import { describe, expect, it } from 'vitest';

import {
  COMPACTION_MARKER_PREFIX,
  applyContextCompaction,
  estimateHistoryTokens,
  isCompactionMarker,
  shouldTrim,
} from './contextTrimmer';
import type { ChatMessage } from './llamaClient';

function userMsg(text: string): ChatMessage {
  return { role: 'user', content: text };
}

function assistantMsg(text: string): ChatMessage {
  return { role: 'assistant', content: text };
}

/** A multi-turn history whose token estimate is comfortably non-zero. */
function sampleHistory(): ChatMessage[] {
  return [
    userMsg('a'.repeat(400)),
    assistantMsg('b'.repeat(400)),
    userMsg('c'.repeat(400)),
    assistantMsg('d'.repeat(400)),
    userMsg('e'.repeat(400)),
    assistantMsg('f'.repeat(400)),
  ];
}

describe('estimateHistoryTokens', () => {
  it('estimates ~1 token per 4 chars of string content', () => {
    expect(estimateHistoryTokens([userMsg('x'.repeat(400))])).toBe(100);
  });

  it('counts image parts with a flat estimate', () => {
    const withImage: ChatMessage = {
      role: 'user',
      content: [
        { type: 'text', text: 'x'.repeat(40) },
        { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA' } },
      ],
    };
    expect(estimateHistoryTokens([withImage])).toBe(10 + 500);
  });
});

describe('shouldTrim', () => {
  it('never trims with an unknown context limit', () => {
    expect(shouldTrim(sampleHistory(), null, 50)).toBe(false);
    expect(shouldTrim(sampleHistory(), 0, 50)).toBe(false);
  });

  it('trims once the estimate crosses the threshold percent', () => {
    const history = sampleHistory(); // ~600 estimated tokens
    expect(shouldTrim(history, 1000, 50)).toBe(true);
    expect(shouldTrim(history, 10000, 50)).toBe(false);
  });
});

describe('applyContextCompaction', () => {
  const failingSummary = () => Promise.reject(new Error('should not be called'));

  it('does nothing with fewer than two user turns', async () => {
    const history = [userMsg('only turn'), assistantMsg('reply')];
    const result = await applyContextCompaction(history, {
      strategy: 'trim',
      contextLimit: 10,
      thresholdPercent: 1,
      sendForSummary: failingSummary,
    });
    expect(result.changed).toBe(false);
    expect(result.messages).toBe(history);
  });

  it('trims the oldest turns and inserts a visible marker', async () => {
    const history = sampleHistory();
    const result = await applyContextCompaction(history, {
      strategy: 'trim',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: failingSummary,
    });
    expect(result.changed).toBe(true);
    expect(result.messages.length).toBeLessThan(history.length);
    const marker = result.messages.find(isCompactionMarker);
    expect(marker).toBeDefined();
    expect(marker!.content).toContain(COMPACTION_MARKER_PREFIX);
    // The most recent messages must survive.
    expect(result.messages[result.messages.length - 1]).toBe(history[history.length - 1]);
  });

  it('uses the model summary for the summarize strategy', async () => {
    const history = sampleHistory();
    const result = await applyContextCompaction(history, {
      strategy: 'summarize',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: async () => 'the earlier chat set up variables a-d',
    });
    expect(result.changed).toBe(true);
    const marker = result.messages.find(isCompactionMarker);
    expect(marker!.content).toContain('the earlier chat set up variables a-d');
  });

  it('falls back to a plain trim marker when summarization fails', async () => {
    const history = sampleHistory();
    const result = await applyContextCompaction(history, {
      strategy: 'summarize',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: () => Promise.reject(new Error('provider down')),
    });
    expect(result.changed).toBe(true);
    const marker = result.messages.find(isCompactionMarker);
    expect(marker!.content).toContain('summarization failed');
  });

  it('drops or summarizes a synthetic notice (e.g. [Verify]/[Checkpoint]) sitting mid-history exactly like any other message', async () => {
    // `applyContextCompaction` never special-cases notice prefixes — it cuts
    // by user-turn boundaries over the raw message array, so a `[Verify]`
    // (or `[Checkpoint]`/`[Memory]`) system notice inside the oldest run of
    // turns is dropped/summarized right along with everything else in that
    // span. This is what makes verify notices interact safely with
    // compaction without any dedicated handling in contextTrimmer.ts.
    const verifyNotice: ChatMessage = { role: 'system', content: `[Verify]${JSON.stringify({ label: 'Lint', kind: 'lint', ok: false, code: 1, output: 'e'.repeat(400), durationMs: 10 })}` };
    const history = [
      userMsg('a'.repeat(400)),
      assistantMsg('b'.repeat(400)),
      verifyNotice,
      userMsg('c'.repeat(400)),
      assistantMsg('d'.repeat(400)),
      userMsg('e'.repeat(400)),
      assistantMsg('f'.repeat(400)),
    ];

    const trimmed = await applyContextCompaction(history, {
      strategy: 'trim',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: failingSummary,
    });
    expect(trimmed.changed).toBe(true);
    expect(trimmed.messages).not.toContain(verifyNotice);

    const summarized = await applyContextCompaction(history, {
      strategy: 'summarize',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: async (dropped) => {
        expect(dropped).toContain(verifyNotice);
        return 'summary covering the verify notice';
      },
    });
    expect(summarized.changed).toBe(true);
    expect(summarized.messages).not.toContain(verifyNotice);
    const marker = summarized.messages.find(isCompactionMarker);
    expect(marker!.content).toContain('summary covering the verify notice');
  });

  it('preserves a leading system prefix', async () => {
    const system: ChatMessage = { role: 'system', content: 'pinned' };
    const history = [system, ...sampleHistory()];
    const result = await applyContextCompaction(history, {
      strategy: 'trim',
      contextLimit: 700,
      thresholdPercent: 85,
      sendForSummary: failingSummary,
    });
    expect(result.changed).toBe(true);
    expect(result.messages[0]).toBe(system);
  });
});
