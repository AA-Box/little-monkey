/**
 * The Talk conversation, with no browser in it.
 *
 * Everything that decides *what happens* — when an utterance has ended, when a
 * transcript becomes a turn, where a sentence may be cut for speech, what
 * counts as talking over the assistant, whether a wake phrase was heard — lives
 * here and is driven by injected ports. The microphone, the recorder and the
 * speaker live in `TalkPanel.tsx`, which supplies those ports.
 *
 * That split is not tidiness. A voice loop is exactly the kind of code that is
 * only ever tested by a person putting their face near a laptop, and the parts
 * most likely to be wrong — barge-in, max-utterance, an interrupted turn's
 * cancellation, a wake phrase that must never leave the machine — are all in
 * this file, where a test can drive them with fake time and fake audio.
 *
 * **A spoken turn is not a special turn.** `submitTurn` hands the finalized
 * transcript to the same durable ingress the composer uses, under the
 * recognition job's own id. There is no voice agent, no voice session and no
 * voice model: the answer streams back through the ordinary session, and this
 * file only decides which parts of it to say out loud.
 */

import {
  AdaptiveVad,
  DEFAULT_VAD_CONFIG,
  IncrementalSpeechChunker,
  normalizeVadConfig,
  type VadConfig,
  type VadFrame,
} from './talkAudio';

export type TalkState =
  | 'idle'
  | 'starting'
  | 'listening'
  | 'transcribing'
  | 'thinking'
  | 'speaking'
  | 'interrupted'
  | 'error';

export type TalkMode = 'push_to_talk' | 'continuous';

/** One recorded utterance, as the recorder produced it. */
export interface TalkRecording {
  blob: Blob;
  mediaType: string;
}

export interface TalkLatencyMetric {
  createdAtMs: number;
  speechDetectionMs: number | null;
  sttMs: number | null;
  modelFirstTokenMs: number | null;
  ttsFirstAudioMs: number | null;
  endToEndMs: number | null;
  interrupted: boolean;
  fallback: boolean;
}

/**
 * Everything the engine cannot do itself. Each one is a seam a test replaces.
 */
export interface TalkPorts {
  /** Open the microphone and begin an utterance. Resolves once audio is flowing. */
  startRecording(): Promise<void>;
  /** Close the current utterance and hand back what was recorded. */
  stopRecording(): Promise<TalkRecording | null>;
  /** The operator's own configured transcription backend. */
  transcribe(recording: TalkRecording, jobId: string): Promise<string>;
  /** Hand a finalized transcript to the ordinary durable ingress. */
  submitTurn(text: string, utteranceId: string): Promise<void>;
  /** Ask the running turn to stop. Best effort — see `interrupt`. */
  cancelTurn(): void;
  /** The operator's own configured speech synthesizer. */
  synthesize(text: string, jobId: string): Promise<{ audioBase64: string; mediaType: string }>;
  /** Queue one synthesized chunk for playback. Resolves when it has been played. */
  play(audioBase64: string, mediaType: string): Promise<void>;
  /** Drop everything queued and stop what is playing, right now. */
  stopPlayback(): void;
  /** Persist one bounded latency sample. Never given audio or a transcript. */
  recordMetric(metric: TalkLatencyMetric): void;
  now(): number;
}

export interface TalkOptions {
  mode?: TalkMode;
  vad?: Partial<VadConfig>;
  /** Local wake-phrase detection. Off unless the operator turned it on. */
  wakePhrase?: string | null;
  /** Skip code blocks when speaking. On by default — see `IncrementalSpeechChunker`. */
  speakCodeBlocks?: boolean;
}

export interface TalkSnapshot {
  state: TalkState;
  mode: TalkMode;
  /** 0–1, for a level meter. Never the audio itself. */
  inputLevel: number;
  transcript: string;
  assistantText: string;
  error: string | null;
  /** True while the microphone is open, whatever the state says. */
  capturing: boolean;
  /** True when a wake phrase must be heard before anything is submitted. */
  awaitingWakePhrase: boolean;
}

/** Words the wake detector should ignore when matching. */
function normalizePhrase(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s]/gu, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/**
 * Whether a locally-transcribed fragment contains the wake phrase, and what was
 * said after it.
 *
 * Returns `null` when the phrase is absent, which is the case that must leave
 * no trace: the fragment is dropped by the caller and never becomes a turn, a
 * log line or an upload.
 */
export function wakePhraseMatch(transcript: string, phrase: string): string | null {
  const haystack = normalizePhrase(transcript);
  const needle = normalizePhrase(phrase);
  if (!needle || !haystack) return null;
  const at = haystack.indexOf(needle);
  if (at < 0) return null;
  return haystack.slice(at + needle.length).trim();
}

export class TalkSession {
  private readonly ports: TalkPorts;
  private readonly vadConfig: VadConfig;
  private readonly vad: AdaptiveVad;
  private readonly chunker = new IncrementalSpeechChunker();
  private readonly wakePhrase: string | null;
  private readonly speakCodeBlocks: boolean;
  private listeners = new Set<(snapshot: TalkSnapshot) => void>();

  private state: TalkState = 'idle';
  private mode: TalkMode;
  private inputLevel = 0;
  private transcript = '';
  private assistantText = '';
  private error: string | null = null;
  private capturing = false;
  private running = false;
  private held = false;

  /** Identity of the utterance being recorded, minted before the audio is sent. */
  private utteranceId: string | null = null;
  private utteranceStartedAt: number | null = null;
  private speechDetectedAt: number | null = null;
  private turnStartedAt: number | null = null;
  private firstTokenAt: number | null = null;
  private firstAudioAt: number | null = null;
  private spokenChunks = 0;
  private interruptedThisTurn = false;
  private fallbackThisTurn = false;
  /** Set by an interruption. Everything the abandoned turn streams afterwards
   * is still shown, and none of it is spoken — the user stopped listening. */
  private turnAbandoned = false;
  /** Serializes synthesis and playback so chunks are spoken in order. */
  private speechQueue: Promise<void> = Promise.resolve();
  private playbackGeneration = 0;

  constructor(ports: TalkPorts, options: TalkOptions = {}) {
    this.ports = ports;
    this.mode = options.mode ?? 'push_to_talk';
    this.vadConfig = normalizeVadConfig(options.vad ?? DEFAULT_VAD_CONFIG);
    this.vad = new AdaptiveVad(this.vadConfig);
    this.wakePhrase = options.wakePhrase?.trim() || null;
    this.speakCodeBlocks = options.speakCodeBlocks ?? false;
  }

  subscribe(listener: (snapshot: TalkSnapshot) => void): () => void {
    this.listeners.add(listener);
    listener(this.snapshot());
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshot(): TalkSnapshot {
    return {
      state: this.state,
      mode: this.mode,
      inputLevel: this.inputLevel,
      transcript: this.transcript,
      assistantText: this.assistantText,
      error: this.error,
      capturing: this.capturing,
      awaitingWakePhrase: this.wakePhrase !== null && this.mode === 'continuous',
    };
  }

  setMode(mode: TalkMode): void {
    if (this.mode === mode) return;
    this.mode = mode;
    // Switching mode never leaves the microphone in the other mode's shape: a
    // held key does not survive into Continuous, and Continuous's open
    // microphone does not survive into push-to-talk.
    this.held = false;
    if (this.running) void this.settleCapture();
    this.emit();
  }

  async start(): Promise<void> {
    if (this.running) return;
    this.running = true;
    this.error = null;
    this.turnAbandoned = false;
    this.setState('starting');
    if (this.mode === 'continuous') await this.beginUtterance();
    else this.setState('listening');
  }

  async stop(): Promise<void> {
    this.running = false;
    this.held = false;
    this.playbackGeneration += 1;
    this.ports.stopPlayback();
    if (this.capturing) await this.ports.stopRecording();
    this.capturing = false;
    this.vad.reset();
    this.chunker.reset();
    this.setState('idle');
  }

  /** Push-to-talk, pressed. */
  async press(): Promise<void> {
    if (!this.running || this.mode !== 'push_to_talk') return;
    this.held = true;
    // Pressing while the assistant is talking is the plainest possible
    // interruption, and it must behave exactly like talking over it.
    if (this.state === 'speaking') this.interrupt('push_to_talk');
    await this.beginUtterance();
  }

  /** Push-to-talk, released. */
  async release(): Promise<void> {
    if (!this.held) return;
    this.held = false;
    await this.finishUtterance('released');
  }

  /**
   * One frame of microphone level, from the panel's analyser.
   *
   * The only thing the engine is told about the audio is how loud it was. That
   * is what makes "raw microphone audio never reaches a log" a property of the
   * design rather than a rule somebody has to remember: this file has never
   * held a sample.
   */
  observeLevel(rms: number, nowMs = this.ports.now()): VadFrame {
    const frame = this.vad.sample(rms, nowMs);
    this.inputLevel = frame.inputLevel;
    if (frame.event === 'speech-start') {
      this.speechDetectedAt = nowMs;
      // Talking over the answer stops it, in Continuous as well as on a press.
      if (this.state === 'speaking') this.interrupt('barge_in');
    }
    // Push-to-talk is bounded by the key, not by silence: a pause mid-sentence
    // must not end an utterance the operator is still holding.
    if (this.mode === 'continuous' && this.capturing) {
      if (frame.event === 'utterance-end') void this.finishUtterance('silence');
      else if (frame.event === 'max-utterance') void this.finishUtterance('max_utterance');
    }
    this.emit();
    return frame;
  }

  /**
   * Talking over the assistant.
   *
   * The order is the honest one: stop the speaker, drop what has not been
   * spoken, mark the turn interrupted, then ask the run to stop. A tool call
   * that already reached the world is not undone by any of it, and nothing here
   * says it was.
   */
  interrupt(reason: string): void {
    if (this.state !== 'speaking' && this.state !== 'thinking') return;
    this.playbackGeneration += 1;
    this.ports.stopPlayback();
    this.chunker.reset();
    this.speechQueue = Promise.resolve();
    this.interruptedThisTurn = true;
    this.turnAbandoned = true;
    this.setState('interrupted');
    this.ports.cancelTurn();
    void reason;
    this.finishTurnMetrics();
    this.setState(this.running ? 'listening' : 'idle');
  }

  /** Streamed assistant text, as the ordinary session produces it. */
  onAssistantDelta(delta: string): void {
    if (!delta) return;
    if (this.firstTokenAt === null) this.firstTokenAt = this.ports.now();
    this.assistantText += delta;
    // An interrupted turn keeps arriving — it is durable, and cancellation is a
    // request, not a guillotine. It stays visible in the transcript and is not
    // spoken: the user is already talking about something else.
    if (this.turnAbandoned) {
      this.emit();
      return;
    }
    if (this.state === 'thinking') this.setState('speaking');
    this.enqueueSpeech(this.chunker.append(delta, false));
    this.emit();
  }

  /** The turn settled. Anything still buffered is spoken, then it is over. */
  onTurnFinished(errorMessage?: string): void {
    if (this.turnAbandoned) {
      // Its metrics were written when it was interrupted; there is nothing left
      // to say and nothing left to time.
      this.turnAbandoned = false;
      if (errorMessage) this.error = errorMessage;
      this.emit();
      return;
    }
    if (errorMessage) {
      this.error = errorMessage;
      this.fallbackThisTurn = true;
    }
    this.enqueueSpeech(this.chunker.append('', true));
    this.speechQueue = this.speechQueue.then(() => {
      this.finishTurnMetrics();
      if (this.running) this.setState('listening');
      else this.setState('idle');
      // Continuous keeps the microphone open between turns; push-to-talk waits
      // for the next press.
      if (this.running && this.mode === 'continuous') void this.beginUtterance();
    });
  }

  // --- internals ---------------------------------------------------------

  private async beginUtterance(): Promise<void> {
    if (this.capturing || !this.running) return;
    try {
      await this.ports.startRecording();
      this.capturing = true;
      this.utteranceId = `talk-${crypto.randomUUID()}`;
      this.utteranceStartedAt = this.ports.now();
      this.speechDetectedAt = null;
      this.vad.reset();
      this.setState('listening');
    } catch (reason) {
      this.fail(reason);
    }
  }

  /** Stop the microphone without submitting — used when the mode changes. */
  private async settleCapture(): Promise<void> {
    if (!this.capturing) return;
    await this.ports.stopRecording();
    this.capturing = false;
    this.vad.reset();
    if (this.mode === 'continuous' && this.running) await this.beginUtterance();
    else this.setState(this.running ? 'listening' : 'idle');
  }

  private async finishUtterance(reason: 'released' | 'silence' | 'max_utterance'): Promise<void> {
    if (!this.capturing) return;
    this.capturing = false;
    const utteranceId = this.utteranceId ?? `talk-${crypto.randomUUID()}`;
    this.utteranceId = null;
    const detectionMs =
      this.speechDetectedAt !== null && this.utteranceStartedAt !== null
        ? this.speechDetectedAt - this.utteranceStartedAt
        : null;
    let recording: TalkRecording | null = null;
    try {
      recording = await this.ports.stopRecording();
    } catch (reason_) {
      this.fail(reason_);
      return;
    }
    this.vad.reset();
    if (!recording || recording.blob.size === 0) {
      // Nothing was said. Silence is not an error and must not become a turn.
      if (this.running && this.mode === 'continuous') await this.beginUtterance();
      else this.setState(this.running ? 'listening' : 'idle');
      return;
    }
    this.setState('transcribing');
    const sttStartedAt = this.ports.now();
    let text: string;
    try {
      text = (await this.ports.transcribe(recording, utteranceId)).trim();
    } catch (reason_) {
      this.fallbackThisTurn = true;
      this.fail(reason_);
      if (this.running && this.mode === 'continuous') await this.beginUtterance();
      return;
    }
    const sttMs = this.ports.now() - sttStartedAt;

    let spoken = text;
    if (this.wakePhrase && this.mode === 'continuous') {
      // Local, and it stops here. A fragment without the phrase is dropped
      // without being recorded, submitted or logged — that is the whole point
      // of doing the detection on this machine.
      const after = wakePhraseMatch(text, this.wakePhrase);
      if (after === null) {
        this.transcript = '';
        if (this.running) await this.beginUtterance();
        return;
      }
      spoken = after;
    }
    if (!spoken) {
      if (this.running && this.mode === 'continuous') await this.beginUtterance();
      else this.setState(this.running ? 'listening' : 'idle');
      return;
    }

    this.transcript = spoken;
    this.assistantText = '';
    this.chunker.reset();
    this.interruptedThisTurn = false;
    this.fallbackThisTurn = false;
    this.turnAbandoned = false;
    // A new turn is the moment a previous turn's failure stops being news.
    this.error = null;
    this.firstTokenAt = null;
    this.firstAudioAt = null;
    this.spokenChunks = 0;
    this.turnStartedAt = this.ports.now();
    this.pendingMetric = {
      speechDetectionMs: detectionMs,
      sttMs,
      startedAt: this.utteranceStartedAt ?? this.turnStartedAt,
    };
    void reason;
    this.setState('thinking');
    try {
      await this.ports.submitTurn(spoken, utteranceId);
    } catch (reason_) {
      this.fallbackThisTurn = true;
      this.fail(reason_);
      if (this.running && this.mode === 'continuous') await this.beginUtterance();
    }
  }

  private pendingMetric: {
    speechDetectionMs: number | null;
    sttMs: number | null;
    startedAt: number;
  } | null = null;

  /**
   * Speak the chunks that just became safe to speak, in order.
   *
   * Chained rather than fired in parallel: two synthesis calls resolving out of
   * order would play the answer's second sentence before its first. The
   * generation counter is what makes an interruption drop everything already
   * queued instead of letting it finish.
   */
  private enqueueSpeech(chunks: string[]): void {
    if (chunks.length === 0) return;
    const generation = this.playbackGeneration;
    for (const chunk of chunks) {
      if (!this.speakCodeBlocks && chunk.length === 0) continue;
      this.speechQueue = this.speechQueue.then(async () => {
        if (generation !== this.playbackGeneration) return;
        try {
          const jobId = `talk-tts-${crypto.randomUUID()}`;
          const audio = await this.ports.synthesize(chunk, jobId);
          if (generation !== this.playbackGeneration) return;
          if (this.firstAudioAt === null) this.firstAudioAt = this.ports.now();
          this.spokenChunks += 1;
          await this.ports.play(audio.audioBase64, audio.mediaType);
        } catch {
          // The answer is on screen either way; only the voice is missing. A
          // synthesizer that is not configured must not lose the conversation.
          this.fallbackThisTurn = true;
        }
      });
    }
  }

  private finishTurnMetrics(): void {
    const pending = this.pendingMetric;
    if (!pending) return;
    this.pendingMetric = null;
    const now = this.ports.now();
    // Durations only, and only bounded ones: no transcript, no answer, no
    // audio. This is what a support bundle is allowed to contain.
    this.ports.recordMetric({
      createdAtMs: now,
      speechDetectionMs: pending.speechDetectionMs,
      sttMs: pending.sttMs,
      modelFirstTokenMs:
        this.firstTokenAt !== null && this.turnStartedAt !== null
          ? this.firstTokenAt - this.turnStartedAt
          : null,
      ttsFirstAudioMs:
        this.firstAudioAt !== null && this.turnStartedAt !== null
          ? this.firstAudioAt - this.turnStartedAt
          : null,
      endToEndMs: now - pending.startedAt,
      interrupted: this.interruptedThisTurn,
      fallback: this.fallbackThisTurn,
    });
  }

  private fail(reason: unknown): void {
    this.error = reason instanceof Error ? reason.message : String(reason);
    this.setState('error');
  }

  private setState(next: TalkState): void {
    if (this.state === next) return;
    this.state = next;
    // The error is deliberately NOT cleared here. A transcription backend that
    // is not configured returns the session to listening, and the operator has
    // to be able to read why nothing happened — it is cleared when the next
    // turn is actually submitted, and on `start`.
    this.emit();
  }

  private emit(): void {
    const snapshot = this.snapshot();
    for (const listener of this.listeners) listener(snapshot);
  }
}
