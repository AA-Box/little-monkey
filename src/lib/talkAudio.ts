export interface VadConfig {
  minSpeechMs: number;
  silenceMs: number;
  maxUtteranceMs: number;
}

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
