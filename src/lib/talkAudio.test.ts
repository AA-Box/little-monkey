import { describe, expect, it } from 'vitest';

import {
  AdaptiveVad,
  DEFAULT_VAD_CONFIG,
  IncrementalSpeechChunker,
  base64AudioBlob,
  normalizeVadConfig,
  rmsOf,
} from './talkAudio';

/** Feed `ms` of audio at `rms` in 20 ms frames, returning the events seen. */
function drive(vad: AdaptiveVad, rms: number, ms: number, startAt: number): string[] {
  const events: string[] = [];
  for (let elapsed = 0; elapsed < ms; elapsed += 20) {
    const frame = vad.sample(rms, startAt + elapsed);
    if (frame.event !== 'none') events.push(frame.event);
  }
  return events;
}

describe('AdaptiveVad', () => {
  it('needs sustained speech before an utterance starts', () => {
    const vad = new AdaptiveVad({ minSpeechMs: 180, silenceMs: 800 });
    // A door, a cough, a chair: loud, and over before 180 ms.
    expect(drive(vad, 0.4, 100, 0)).toEqual([]);
    drive(vad, 0.001, 400, 100);
    expect(drive(vad, 0.4, 260, 500)).toEqual(['speech-start']);
  });

  it('ends an utterance on the configured silence and not before', () => {
    const vad = new AdaptiveVad({ minSpeechMs: 180, silenceMs: 800 });
    drive(vad, 0.4, 300, 0);
    // A pause mid-sentence is not the end of one.
    expect(drive(vad, 0.001, 600, 300)).toEqual([]);
    expect(drive(vad, 0.4, 200, 900)).toEqual([]);
    expect(drive(vad, 0.001, 900, 1_100)).toEqual(['utterance-end']);
  });

  it('cuts a monologue at the maximum utterance rather than listening for ever', () => {
    const vad = new AdaptiveVad({ minSpeechMs: 180, silenceMs: 800, maxUtteranceMs: 5_000 });
    const events = drive(vad, 0.4, 6_000, 0);
    expect(events).toContain('speech-start');
    expect(events).toContain('max-utterance');
  });

  it('adapts to a noisy room instead of hearing the room as speech', () => {
    const quiet = new AdaptiveVad({ minSpeechMs: 180 });
    const noisy = new AdaptiveVad({ minSpeechMs: 180 });
    // A room whose floor is already at 0.02 — a fan, a café — settles above it.
    drive(noisy, 0.02, 4_000, 0);
    const level = 0.05;
    expect(drive(quiet, level, 400, 10_000)).toEqual(['speech-start']);
    expect(drive(noisy, level, 400, 10_000)).toEqual([]);
  });

  it('bounds every configurable value to the documented range', () => {
    expect(normalizeVadConfig({})).toEqual(DEFAULT_VAD_CONFIG);
    const clamped = normalizeVadConfig({
      minSpeechMs: 5,
      silenceMs: 50,
      maxUtteranceMs: 10 * 60_000,
    });
    expect(clamped.minSpeechMs).toBe(80);
    expect(clamped.silenceMs).toBe(400);
    expect(clamped.maxUtteranceMs).toBe(90_000);
    expect(normalizeVadConfig({ silenceMs: 9_000 }).silenceMs).toBe(2_000);
  });

  it('reports a level for a meter without being given the audio', () => {
    expect(rmsOf(new Float32Array())).toBe(0);
    expect(rmsOf(new Float32Array([1, -1, 1, -1]))).toBeCloseTo(1);
    expect(rmsOf(new Float32Array([0, 0]))).toBe(0);
  });
});

describe('IncrementalSpeechChunker', () => {
  it('speaks a finished sentence before the answer is complete', () => {
    const chunker = new IncrementalSpeechChunker();
    expect(chunker.append('The deploy fin', false)).toEqual([]);
    expect(chunker.append('ished. And then', false)).toEqual(['The deploy finished.']);
    expect(chunker.append('', true)).toEqual(['And then']);
  });

  it('never lets a fenced code block reach the synthesizer', () => {
    const chunker = new IncrementalSpeechChunker();
    const spoken = [
      ...chunker.append('Here is the fix:\n', false),
      ...chunker.append('```rust\nfn main() { panic!() }\n```\n', false),
      ...chunker.append('That is all. ', true),
    ].join(' ');
    expect(spoken).toContain('Here is the fix');
    expect(spoken).not.toContain('fn main');
    expect(spoken).not.toContain('panic');
  });

  it('drops URLs rather than reading them out character by character', () => {
    const chunker = new IncrementalSpeechChunker();
    const spoken = chunker.append('See https://example.com/deploy?x=1 for detail. ', true);
    expect(spoken.join(' ')).not.toContain('http');
    expect(spoken.join(' ')).toContain('for detail');
  });

  it('waits for a half-written link instead of speaking its punctuation', () => {
    const chunker = new IncrementalSpeechChunker();
    expect(chunker.append('Read the [release notes', false)).toEqual([]);
    const spoken = chunker.append('](https://example.com). ', false).join(' ');
    expect(spoken).toContain('Read the release notes');
    expect(spoken).not.toContain('](');
  });

  it('holds a partial fence rather than speaking a stray backtick', () => {
    const chunker = new IncrementalSpeechChunker();
    // The delta boundary lands inside the fence marker.
    expect(chunker.append('Try this: `', false)).toEqual([]);
    expect(chunker.append('``\nsecret\n```\ndone. ', true).join(' ')).not.toContain('secret');
  });
});

describe('base64AudioBlob', () => {
  it('round-trips synthesized audio into a playable blob', async () => {
    const blob = base64AudioBlob(btoa('RIFFfake'), 'audio/wav');
    expect(blob.type).toBe('audio/wav');
    expect(await blob.text()).toBe('RIFFfake');
    // An unnamed media type still produces something a player can be handed.
    expect(base64AudioBlob(btoa('x'), '').type).toBe('audio/wav');
  });
});
