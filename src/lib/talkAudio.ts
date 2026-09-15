export interface VadConfig {
  minSpeechMs: number;
  silenceMs: number;
  maxUtteranceMs: number;
}

/** The only browser microphone processor used by Talk and the settings test.
 * It batches raw mono floats; resampling, bounds and native inference remain in
 * ordinary testable code outside the real-time audio thread. */
export const PCM_AUDIO_WORKLET_SOURCE = `
class LittleMonkeyPcmCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.pending = [];
  }
  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (!channel || channel.length === 0) return true;
    for (let i = 0; i < channel.length; i += 1) this.pending.push(channel[i]);
    if (this.pending.length >= 2048) {
      const frame = Float32Array.from(this.pending);
      this.pending = [];
      this.port.postMessage(frame, [frame.buffer]);
    }
    return true;
  }
}
registerProcessor('little-monkey-pcm-capture', LittleMonkeyPcmCapture);
`;

export type VadEvent = "none" | "speech-start" | "utterance-end" | "max-utterance";

export interface VadFrame {
  event: VadEvent;
  inputLevel: number;
  noiseFloor: number;
  threshold: number;
  speaking: boolean;
}

export const DEFAULT_VAD_CONFIG: VadConfig = {
  minSpeechMs: 180,
  silenceMs: 800,
  maxUtteranceMs: 90_000,
};

/**
 * The bounds are the ones `validate_config` enforces on the Rust side, to the
 * millisecond. A narrower clamp here would quietly run a machine at settings
 * the operator never chose and the settings screen still shows: a saved 100 ms
 * minimum would have become 80, and a saved 2 s monologue limit 5 s.
 */
export function normalizeVadConfig(config: Partial<VadConfig>): VadConfig {
  return {
    minSpeechMs: Math.min(Math.max(Math.round(config.minSpeechMs ?? DEFAULT_VAD_CONFIG.minSpeechMs), 50), 2_000),
    silenceMs: Math.min(Math.max(Math.round(config.silenceMs ?? DEFAULT_VAD_CONFIG.silenceMs), 400), 2_000),
    maxUtteranceMs: Math.min(Math.max(Math.round(config.maxUtteranceMs ?? DEFAULT_VAD_CONFIG.maxUtteranceMs), 1_000), 90_000),
  };
}

export function rmsOf(samples: Float32Array): number {
  if (samples.length === 0) return 0;
  let squares = 0;
  for (const sample of samples) squares += sample * sample;
  return Math.sqrt(squares / samples.length);
}

/** Stateful linear resampler for consecutive AudioWorklet chunks. It keeps the
 * fractional source position across calls, so chunk boundaries cannot insert
 * or drop a sample. */
export class StreamingLinearResampler {
  private sourceRate = 0;
  private targetRate: number;
  private previous: number | null = null;
  private position = 0;

  constructor(targetRate = 16_000) {
    if (!Number.isFinite(targetRate) || targetRate <= 0) throw new Error('Invalid target sample rate');
    this.targetRate = targetRate;
  }

  reset(): void {
    this.sourceRate = 0;
    this.previous = null;
    this.position = 0;
  }

  process(input: Float32Array, sourceRate: number): Float32Array {
    if (input.length === 0) return new Float32Array();
    if (!Number.isFinite(sourceRate) || sourceRate <= 0) throw new Error('Invalid source sample rate');
    if (this.sourceRate !== 0 && this.sourceRate !== sourceRate) this.reset();
    this.sourceRate = sourceRate;
    if (sourceRate === this.targetRate) {
      this.previous = input[input.length - 1];
      return input.slice();
    }

    const combined = new Float32Array(input.length + (this.previous === null ? 0 : 1));
    let offset = 0;
    if (this.previous !== null) {
      combined[0] = this.previous;
      offset = 1;
    }
    combined.set(input, offset);
    const step = sourceRate / this.targetRate;
    const output: number[] = [];
    while (this.position < combined.length - 1) {
      const left = Math.floor(this.position);
      const mix = this.position - left;
      output.push(combined[left] * (1 - mix) + combined[left + 1] * mix);
      this.position += step;
    }
    this.position -= combined.length - 1;
    this.previous = combined[combined.length - 1];
    return Float32Array.from(output);
  }
}

/** Fixed-capacity PCM history addressed by the absolute number of samples seen.
 * It is the bridge between the detector's keyword-end timestamp and the command
 * recorder: only samples after the keyword are copied into Whisper's clip. */
export class PcmRingBuffer {
  private readonly samples: Float32Array;
  private written = 0;

  constructor(capacity: number) {
    if (!Number.isInteger(capacity) || capacity <= 0) throw new Error('Invalid PCM ring capacity');
    this.samples = new Float32Array(capacity);
  }

  get totalWritten(): number {
    return this.written;
  }

  write(frame: Float32Array): void {
    for (const sample of frame) {
      this.samples[this.written % this.samples.length] = sample;
      this.written += 1;
    }
  }

  sliceFrom(absoluteStart: number): Float32Array {
    const earliest = Math.max(0, this.written - this.samples.length);
    const start = Math.min(this.written, Math.max(earliest, Math.floor(absoluteStart)));
    const output = new Float32Array(this.written - start);
    for (let index = 0; index < output.length; index += 1) {
      output[index] = this.samples[(start + index) % this.samples.length];
    }
    return output;
  }
}

/** One bounded handoff slot for native KWS. When inference is slower than the
 * microphone, old pending audio is dropped instead of growing an unbounded
 * queue; the native metrics receive the exact drop count on stop. */
export class BoundedPcmQueue {
  private pending = new Float32Array();
  droppedFrames = 0;

  constructor(private readonly capacity: number) {}

  enqueue(frame: Float32Array): void {
    const merged = new Float32Array(this.pending.length + frame.length);
    merged.set(this.pending);
    merged.set(frame, this.pending.length);
    if (merged.length <= this.capacity) {
      this.pending = merged;
      return;
    }
    this.droppedFrames += 1;
    this.pending = merged.slice(merged.length - this.capacity);
  }

  take(): Float32Array {
    const value = this.pending;
    this.pending = new Float32Array();
    return value;
  }

  get length(): number {
    return this.pending.length;
  }
}

/** Encode transient 16 kHz mono PCM as a WAV Blob for the existing local
 * Whisper entry point. The float samples are never persisted by this helper. */
export function pcm16WavBlob(samples: Float32Array, sampleRate = 16_000): Blob {
  const buffer = new ArrayBuffer(44 + samples.length * 2);
  const view = new DataView(buffer);
  const ascii = (offset: number, value: string) => {
    for (let index = 0; index < value.length; index += 1) view.setUint8(offset + index, value.charCodeAt(index));
  };
  ascii(0, 'RIFF');
  view.setUint32(4, 36 + samples.length * 2, true);
  ascii(8, 'WAVE');
  ascii(12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  ascii(36, 'data');
  view.setUint32(40, samples.length * 2, true);
  for (let index = 0; index < samples.length; index += 1) {
    const sample = Math.max(-1, Math.min(1, samples[index]));
    view.setInt16(44 + index * 2, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true);
  }
  return new Blob([buffer], { type: 'audio/wav' });
}

/** Adaptive, local-only speech detector. Quiet frames update the rolling
 * ambient floor; speech must remain above that floor for `minSpeechMs` before
 * it becomes an utterance. */
export class AdaptiveVad {
  private readonly config: VadConfig;
  private noiseFloor = 0.008;
  private candidateStartedAt: number | null = null;
  private speechStartedAt: number | null = null;
  private lastSpeechAt: number | null = null;

  constructor(config: Partial<VadConfig> = {}) {
    this.config = normalizeVadConfig(config);
  }

  reset(): void {
    this.candidateStartedAt = null;
    this.speechStartedAt = null;
    this.lastSpeechAt = null;
  }

  sample(rms: number, nowMs: number): VadFrame {
    const safeRms = Number.isFinite(rms) ? Math.max(0, rms) : 0;
    const threshold = Math.max(0.012, this.noiseFloor * 2.8);
    const aboveThreshold = safeRms >= threshold;
    let event: VadEvent = "none";

    if (this.speechStartedAt === null) {
      if (aboveThreshold) {
        this.candidateStartedAt ??= nowMs;
        if (nowMs - this.candidateStartedAt >= this.config.minSpeechMs) {
          this.speechStartedAt = this.candidateStartedAt;
          this.lastSpeechAt = nowMs;
          event = "speech-start";
        }
      } else {
        this.candidateStartedAt = null;
        this.updateNoiseFloor(safeRms);
      }
    } else {
      if (aboveThreshold) this.lastSpeechAt = nowMs;
      if (nowMs - this.speechStartedAt >= this.config.maxUtteranceMs) {
        event = "max-utterance";
        this.reset();
      } else if (this.lastSpeechAt !== null && nowMs - this.lastSpeechAt >= this.config.silenceMs) {
        event = "utterance-end";
        this.reset();
      }
    }

    return {
      event,
      inputLevel: Math.min(1, safeRms / Math.max(threshold * 2.5, 0.001)),
      noiseFloor: this.noiseFloor,
      threshold,
      speaking: this.speechStartedAt !== null,
    };
  }

  private updateNoiseFloor(rms: number): void {
    const bounded = Math.min(Math.max(rms, 0.0005), 0.08);
    this.noiseFloor = this.noiseFloor * 0.96 + bounded * 0.04;
  }
}

function stripMarkdownForSpeech(value: string): string {
  return value
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/https?:\/\/\S+/gi, "")
    .replace(/<[^>]+>/g, " ")
    .replace(/(^|\s)[#>]+\s*/g, "$1")
    .replace(/[*_~`]+/g, "")
    .replace(/[\[\]()]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function incompleteMarkdownStartsAt(value: string): number | null {
  const openBracket = value.lastIndexOf("[");
  const closeBracket = value.lastIndexOf("]");
  if (openBracket > closeBracket) return openBracket;
  const linkStart = value.lastIndexOf("](");
  if (linkStart >= 0 && value.indexOf(")", linkStart + 2) < 0) {
    return value.lastIndexOf("[", linkStart);
  }
  return null;
}

/** Turns streamed Markdown into sentence/phrase-sized TTS chunks. Fenced code
 * never enters the speech buffer, and incomplete links/URLs wait for more
 * input instead of being read as malformed markup. */
export class IncrementalSpeechChunker {
  private speechBuffer = "";
  private tickCarry = "";
  private inCodeFence = false;

  append(delta: string, final = false): string[] {
    this.ingest(delta, final);
    return this.drain(final);
  }

  reset(): void {
    this.speechBuffer = "";
    this.tickCarry = "";
    this.inCodeFence = false;
  }

  private ingest(delta: string, final: boolean): void {
    const value = this.tickCarry + delta;
    this.tickCarry = "";
    let index = 0;
    while (index < value.length) {
      if (value.startsWith("```", index)) {
        this.inCodeFence = !this.inCodeFence;
        index += 3;
        continue;
      }
      const remaining = value.length - index;
      if (!final && value[index] === "`" && remaining < 3) {
        this.tickCarry = value.slice(index);
        break;
      }
      if (!this.inCodeFence) this.speechBuffer += value[index];
      index += 1;
    }
    if (final) {
      if (!this.inCodeFence) this.speechBuffer += this.tickCarry;
      this.tickCarry = "";
    }
  }

  private drain(final: boolean): string[] {
    const chunks: string[] = [];
    while (this.speechBuffer.length > 0) {
      const incompleteAt = incompleteMarkdownStartsAt(this.speechBuffer);
      const scanLimit = incompleteAt ?? this.speechBuffer.length;
      let boundary = -1;
      for (let index = 0; index < scanLimit; index += 1) {
        const char = this.speechBuffer[index];
        const next = this.speechBuffer[index + 1] ?? "";
        const sentence = /[.!?]/.test(char) && (next === "" || /\s/.test(next));
        const phrase = /[;:]/.test(char) && /\s/.test(next) && index >= 48;
        const line = char === "\n";
        const longClause = char === "," && /\s/.test(next) && index >= 180;
        if (sentence || phrase || line || longClause) boundary = index + 1;
        if (boundary > 0 && boundary >= 320) break;
      }
      if (boundary < 0 && final) boundary = scanLimit;
      if (boundary <= 0) break;

      const raw = this.speechBuffer.slice(0, boundary);
      this.speechBuffer = this.speechBuffer.slice(boundary).replace(/^\s+/, "");
      const clean = stripMarkdownForSpeech(raw);
      if (clean) chunks.push(clean);
    }
    if (final && this.inCodeFence) this.inCodeFence = false;
    return chunks;
  }
}

export function base64AudioBlob(audioBase64: string, mediaType: string): Blob {
  const binary = atob(audioBase64);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return new Blob([bytes], { type: mediaType || "audio/wav" });
}
