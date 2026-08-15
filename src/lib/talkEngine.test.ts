import { beforeEach, describe, expect, it, vi } from 'vitest';

import {
  TalkSession,
  wakePhraseMatch,
  type TalkLatencyMetric,
  type TalkPorts,
  type TalkRecording,
  type TalkState,
} from './talkEngine';

/**
 * A fake microphone, transcriber, queue and speaker.
 *
 * Everything a Talk session touches outside itself, recorded so a test can ask
 * what actually happened rather than what the UI looked like.
 */
class Harness implements TalkPorts {
  clock = 0;
  recordings: TalkRecording[] = [];
  heard: string[] = [];
  submitted: Array<{ text: string; utteranceId: string }> = [];
  synthesized: string[] = [];
  played: string[] = [];
  stopPlaybackCalls = 0;
  cancelled = 0;
  metrics: TalkLatencyMetric[] = [];
  recording = false;
  /** What `transcribe` returns next, in order. */
  transcripts: string[] = [];
  /** Bytes the next `stopRecording` hands back. Zero means silence. */
  nextBlobSize = 1_024;
  failTranscription = false;
  failSynthesis = false;

  now() {
    return this.clock;
  }

  async startRecording() {
    this.recording = true;
  }

  async stopRecording() {
    if (!this.recording) return null;
    this.recording = false;
    if (this.nextBlobSize === 0) return null;
    const recording = {
      blob: new Blob([new Uint8Array(this.nextBlobSize)], { type: 'audio/webm' }),
      mediaType: 'audio/webm',
    };
    this.recordings.push(recording);
    return recording;
  }

  async transcribe(recording: TalkRecording) {
    if (this.failTranscription) throw new Error('no transcription backend is configured');
    void recording;
    const next = this.transcripts.shift() ?? '';
    this.heard.push(next);
    return next;
  }

  async submitTurn(text: string, utteranceId: string) {
    this.submitted.push({ text, utteranceId });
  }

  cancelTurn() {
    this.cancelled += 1;
  }

  async synthesize(text: string) {
    if (this.failSynthesis) throw new Error('no speech backend is configured');
    this.synthesized.push(text);
    return { audioBase64: btoa(text), mediaType: 'audio/wav' };
  }

  async play(audioBase64: string) {
    this.played.push(atob(audioBase64));
  }

  stopPlayback() {
    this.stopPlaybackCalls += 1;
  }

  recordMetric(metric: TalkLatencyMetric) {
    this.metrics.push(metric);
  }
}

/** Drive `ms` of audio at `rms` through the session's detector. */
function speak(session: TalkSession, harness: Harness, rms: number, ms: number): void {
  for (let elapsed = 0; elapsed < ms; elapsed += 20) {
    harness.clock += 20;
    session.observeLevel(rms, harness.clock);
  }
}

/** Let every chained promise in the speech queue settle. */
async function settle(): Promise<void> {
  for (let index = 0; index < 20; index += 1) await Promise.resolve();
}

beforeEach(() => {
  vi.stubGlobal('crypto', {
    ...globalThis.crypto,
    randomUUID: (() => {
      let counter = 0;
      return () => `uuid-${(counter += 1)}`;
    })(),
  });
});

describe('TalkSession — push to talk', () => {
  it('records while held, submits one turn on release, and speaks the answer', async () => {
    const harness = new Harness();
    harness.transcripts = ['what is the deploy status'];
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    const states: TalkState[] = [];
    session.subscribe((snapshot) => {
      if (states[states.length - 1] !== snapshot.state) states.push(snapshot.state);
    });

    await session.start();
    await session.press();
    expect(harness.recording).toBe(true);
    speak(session, harness, 0.4, 400);
    // A silence longer than the VAD's end-of-utterance must NOT end a held
    // press: push-to-talk is bounded by the key, not by a pause.
    speak(session, harness, 0.001, 1_500);
    expect(harness.submitted).toHaveLength(0);
    expect(harness.recording).toBe(true);

    await session.release();
    expect(harness.submitted).toEqual([
      { text: 'what is the deploy status', utteranceId: 'talk-uuid-1' },
    ]);
    expect(session.snapshot().transcript).toBe('what is the deploy status');

    session.onAssistantDelta('The deploy finished. ');
    session.onAssistantDelta('Nothing is pending.');
    session.onTurnFinished();
    await settle();

    expect(harness.synthesized).toEqual(['The deploy finished.', 'Nothing is pending.']);
    expect(harness.played).toEqual(['The deploy finished.', 'Nothing is pending.']);
    expect(states).toEqual([
      'idle',
      'starting',
      'listening',
      'transcribing',
      'thinking',
      'speaking',
      'listening',
    ]);
  });

  it('never submits a turn for silence', async () => {
    const harness = new Harness();
    harness.nextBlobSize = 0;
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    await session.start();
    await session.press();
    await session.release();
    expect(harness.submitted).toHaveLength(0);
    expect(session.snapshot().state).toBe('listening');
  });

  it('never submits a turn for an empty transcript', async () => {
    const harness = new Harness();
    harness.transcripts = ['   '];
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    await session.start();
    await session.press();
    await session.release();
    expect(harness.submitted).toHaveLength(0);
  });
});

describe('TalkSession — continuous', () => {
  it('ends an utterance on silence and reopens the microphone for the next one', async () => {
    const harness = new Harness();
    harness.transcripts = ['first question', 'second question'];
    const session = new TalkSession(harness, { mode: 'continuous' });
    await session.start();
    expect(harness.recording).toBe(true);

    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();
    expect(harness.submitted.map((turn) => turn.text)).toEqual(['first question']);

    session.onAssistantDelta('Done.');
    session.onTurnFinished();
    await settle();
    // The microphone is open again without anyone pressing anything.
    expect(harness.recording).toBe(true);

    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();
    expect(harness.submitted.map((turn) => turn.text)).toEqual([
      'first question',
      'second question',
    ]);
  });

  it('cuts a monologue at the maximum utterance and answers it', async () => {
    const harness = new Harness();
    harness.transcripts = ['a very long monologue'];
    const session = new TalkSession(harness, {
      mode: 'continuous',
      vad: { maxUtteranceMs: 5_000 },
    });
    await session.start();
    speak(session, harness, 0.4, 6_000);
    await settle();
    expect(harness.submitted.map((turn) => turn.text)).toEqual(['a very long monologue']);
  });
});

describe('TalkSession — barge-in', () => {
  it('stops the speaker, drops what is queued and cancels the run', async () => {
    const harness = new Harness();
    harness.transcripts = ['keep going'];
    const session = new TalkSession(harness, { mode: 'continuous' });
    await session.start();
    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();

    session.onAssistantDelta('One. ');
    await settle();
    expect(session.snapshot().state).toBe('speaking');

    // The user starts talking while it is answering.
    speak(session, harness, 0.4, 300);
    expect(harness.stopPlaybackCalls).toBeGreaterThan(0);
    expect(harness.cancelled).toBe(1);

    // Everything the run streams after the interruption is not spoken.
    const spokenBefore = harness.synthesized.length;
    session.onAssistantDelta('Two. Three. Four. ');
    session.onTurnFinished();
    await settle();
    expect(harness.synthesized).toHaveLength(spokenBefore);
    expect(harness.metrics[harness.metrics.length - 1]?.interrupted).toBe(true);
  });

  it('treats a push-to-talk press during the answer as an interruption', async () => {
    const harness = new Harness();
    harness.transcripts = ['first', 'second'];
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    await session.start();
    await session.press();
    await session.release();
    session.onAssistantDelta('A long answer. ');
    await settle();

    await session.press();
    expect(harness.cancelled).toBe(1);
    expect(harness.stopPlaybackCalls).toBeGreaterThan(0);
    expect(harness.recording).toBe(true);
  });
});

describe('TalkSession — wake phrase', () => {
  it('drops everything that does not contain the phrase, without submitting it', async () => {
    const harness = new Harness();
    harness.transcripts = [
      'the weather is nice today',
      'hey little monkey what is the deploy status',
    ];
    const session = new TalkSession(harness, {
      mode: 'continuous',
      wakePhrase: 'hey little monkey',
    });
    await session.start();

    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();
    expect(harness.submitted).toHaveLength(0);
    expect(session.snapshot().transcript).toBe('');

    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();
    // Only what was said *after* the phrase becomes the turn.
    expect(harness.submitted.map((turn) => turn.text)).toEqual(['what is the deploy status']);
  });

  it('matches the phrase through punctuation and casing', () => {
    expect(wakePhraseMatch('Hey, Little Monkey! deploy please', 'hey little monkey')).toBe(
      'deploy please',
    );
    expect(wakePhraseMatch('hey little monkeys are great', 'hey little monkey')).toBe('s are great');
    expect(wakePhraseMatch('nothing here', 'hey little monkey')).toBeNull();
    expect(wakePhraseMatch('anything', '')).toBeNull();
  });

  it('is inert in push-to-talk, where the press is the wake signal', async () => {
    const harness = new Harness();
    harness.transcripts = ['no phrase in this one'];
    const session = new TalkSession(harness, {
      mode: 'push_to_talk',
      wakePhrase: 'hey little monkey',
    });
    await session.start();
    await session.press();
    await session.release();
    expect(harness.submitted.map((turn) => turn.text)).toEqual(['no phrase in this one']);
  });
});

describe('TalkSession — failures and telemetry', () => {
  it('reports a missing transcription backend and keeps listening', async () => {
    const harness = new Harness();
    harness.failTranscription = true;
    const session = new TalkSession(harness, { mode: 'continuous' });
    await session.start();
    speak(session, harness, 0.4, 400);
    speak(session, harness, 0.001, 1_000);
    await settle();
    expect(session.snapshot().error).toContain('transcription backend');
    expect(harness.submitted).toHaveLength(0);
    // The microphone reopened: a misconfigured backend is a message, not the
    // end of the conversation.
    expect(harness.recording).toBe(true);
  });

  it('keeps the answer on screen when synthesis fails, and records the fallback', async () => {
    const harness = new Harness();
    harness.transcripts = ['question'];
    harness.failSynthesis = true;
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    await session.start();
    await session.press();
    await session.release();
    session.onAssistantDelta('The answer is here. ');
    session.onTurnFinished();
    await settle();

    expect(harness.played).toHaveLength(0);
    expect(session.snapshot().assistantText).toBe('The answer is here. ');
    expect(harness.metrics[harness.metrics.length - 1]?.fallback).toBe(true);
  });

  it('persists durations and nothing that was said', async () => {
    const harness = new Harness();
    harness.transcripts = ['what is the deploy status'];
    const session = new TalkSession(harness, { mode: 'push_to_talk' });
    await session.start();
    await session.press();
    speak(session, harness, 0.4, 400);
    await session.release();
    harness.clock += 250;
    session.onAssistantDelta('Finished. ');
    harness.clock += 100;
    session.onTurnFinished();
    await settle();

    expect(harness.metrics).toHaveLength(1);
    const metric = harness.metrics[0];
    expect(metric.sttMs).not.toBeNull();
    expect(metric.modelFirstTokenMs).toBeGreaterThanOrEqual(0);
    expect(metric.endToEndMs).toBeGreaterThan(0);
    // The property the whole telemetry design exists for.
    const serialized = JSON.stringify(harness.metrics);
    expect(serialized).not.toContain('deploy');
    expect(serialized).not.toContain('Finished');
    expect(Object.keys(metric).sort()).toEqual([
      'createdAtMs',
      'endToEndMs',
      'fallback',
      'interrupted',
      'modelFirstTokenMs',
      'speechDetectionMs',
      'sttMs',
      'ttsFirstAudioMs',
    ]);
  });

  it('closes the microphone on stop and on a mode change', async () => {
    const harness = new Harness();
    const session = new TalkSession(harness, { mode: 'continuous' });
    await session.start();
    expect(harness.recording).toBe(true);

    session.setMode('push_to_talk');
    await settle();
    expect(harness.recording).toBe(false);

    await session.press();
    expect(harness.recording).toBe(true);
    await session.stop();
    expect(harness.recording).toBe(false);
    expect(session.snapshot().state).toBe('idle');
  });
});
