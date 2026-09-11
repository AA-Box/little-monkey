import { realtimeVoiceClient, type RealtimeVoiceMediaBridge } from './companionClient';

const TOKEN_HEADER = 'x-little-monkey-host-media-token';
const MAX_CHUNK_BYTES = 64 * 1024;
const MAX_PENDING_BYTES = 512 * 1024;
const MAX_PENDING_CHUNKS = 32;
const POLL_IDLE_MS = 12;

function validateDescriptor(value: RealtimeVoiceMediaBridge): void {
  if (value.protocolVersion !== 1) throw new Error('Unsupported Realtime media bridge protocol.');
  const url = new URL(value.baseUrl);
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) {
    throw new Error('Realtime media bridge is not loopback-only.');
  }
  if (!/^[0-9a-f]{64}$/.test(value.token)) throw new Error('Realtime media bridge token is invalid.');
}

function pcm16Bytes(samples: Float32Array, sourceRate: number): Uint8Array {
  if (!samples.length || !Number.isFinite(sourceRate) || sourceRate < 8_000 || sourceRate > 192_000) return new Uint8Array();
  const targetRate = 24_000;
  const outputLength = Math.max(1, Math.round(samples.length * targetRate / sourceRate));
  const bytes = new Uint8Array(outputLength * 2);
  const view = new DataView(bytes.buffer);
  for (let index = 0; index < outputLength; index += 1) {
    const position = index * sourceRate / targetRate;
    const left = Math.min(samples.length - 1, Math.floor(position));
    const right = Math.min(samples.length - 1, left + 1);
    const fraction = position - left;
    const sample = Math.max(-1, Math.min(1, samples[left] + (samples[right] - samples[left]) * fraction));
    view.setInt16(index * 2, sample < 0 ? Math.round(sample * 0x8000) : Math.round(sample * 0x7fff), true);
  }
  return bytes;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let at = 0; at < bytes.length; at += 0x4000) {
    binary += String.fromCharCode(...bytes.subarray(at, Math.min(bytes.length, at + 0x4000)));
  }
  return btoa(binary);
}

export class RealtimeRouteBridge {
  private descriptor: RealtimeVoiceMediaBridge | null = null;
  private stopped = false;
  private inputAbort: AbortController | null = null;
  private outputTail: Promise<void> = Promise.resolve();
  private pendingOutputBytes = 0;
  private pendingOutputChunks = 0;
  private inputSequence = 0;
  private inputIdleSequence = 0;
  private inputIdleWaiters = new Set<() => void>();
  private outputPaused = false;

  constructor(
    readonly sessionId: string,
    readonly generation: number,
    private readonly onError: (error: Error) => void,
  ) {}

  private async config(): Promise<RealtimeVoiceMediaBridge> {
    if (this.descriptor) return this.descriptor;
    const descriptor = await realtimeVoiceClient.mediaBridge();
    validateDescriptor(descriptor);
    this.descriptor = descriptor;
    return descriptor;
  }

  private async url(direction: 'input' | 'output'): Promise<{ url: string; token: string }> {
    const descriptor = await this.config();
    const base = descriptor.baseUrl.replace(/\/$/, '');
    return {
      url: `${base}/v1/host/realtime/${encodeURIComponent(this.sessionId)}/${this.generation}/${direction}`,
      token: descriptor.token,
    };
  }

  async startInput(onPcm16: (audioBase64: string) => void, shouldForward: () => boolean): Promise<void> {
    this.inputAbort?.abort();
    const abort = new AbortController();
    this.inputAbort = abort;
    this.stopped = false;
    const endpoint = await this.url('input');
    void (async () => {
      while (!this.stopped && !abort.signal.aborted) {
        try {
          const response = await fetch(endpoint.url, {
            method: 'GET', cache: 'no-store', signal: abort.signal,
            headers: { [TOKEN_HEADER]: endpoint.token },
          });
          if (response.status === 204) {
            this.inputIdleSequence += 1;
            for (const wake of [...this.inputIdleWaiters]) wake();
            await new Promise((resolve) => setTimeout(resolve, POLL_IDLE_MS));
            continue;
          }
          if (!response.ok) throw new Error(`Realtime paired microphone bridge returned HTTP ${response.status}.`);
          const bytes = new Uint8Array(await response.arrayBuffer());
          if (!bytes.length || bytes.length > MAX_CHUNK_BYTES || bytes.length % 2 !== 0) {
            throw new Error('Realtime paired microphone returned an invalid PCM16 chunk.');
          }
          const sequence = Number(response.headers.get('x-little-monkey-audio-sequence') || 0);
          if (!Number.isSafeInteger(sequence) || sequence <= this.inputSequence) {
            throw new Error('Realtime paired microphone sequence is stale or invalid.');
          }
          this.inputSequence = sequence;
          if (shouldForward()) onPcm16(bytesToBase64(bytes));
        } catch (reason) {
          if (abort.signal.aborted || this.stopped) return;
          this.onError(reason instanceof Error ? reason : new Error(String(reason)));
          return;
        }
      }
    })();
  }

  async waitForInputIdle(timeoutMs = 1_000): Promise<void> {
    if (this.stopped) throw new Error('Realtime paired microphone bridge is stopped.');
    const after = this.inputIdleSequence;
    if (timeoutMs <= 0) throw new Error('Realtime paired microphone drain timeout must be positive.');
    await new Promise<void>((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout>;
      const check = () => {
        if (this.inputIdleSequence <= after) return;
        cleanup();
        resolve();
      };
      const cleanup = () => {
        clearTimeout(timer);
        this.inputIdleWaiters.delete(check);
      };
      timer = setTimeout(() => {
        cleanup();
        reject(new Error('Timed out draining the paired Realtime microphone before commit.'));
      }, timeoutMs);
      this.inputIdleWaiters.add(check);
    });
  }

  pushOutput(samples: Float32Array, sampleRate: number): void {
    if (this.stopped || this.outputPaused) return;
    const bytes = pcm16Bytes(samples, sampleRate);
    if (!bytes.length) return;
    if (bytes.length > MAX_CHUNK_BYTES
        || this.pendingOutputChunks >= MAX_PENDING_CHUNKS
        || this.pendingOutputBytes + bytes.length > MAX_PENDING_BYTES) {
      this.onError(new Error('Realtime paired speaker bridge reached its bounded backpressure limit.'));
      return;
    }
    this.pendingOutputChunks += 1;
    this.pendingOutputBytes += bytes.length;
    this.outputTail = this.outputTail.catch(() => undefined).then(async () => {
      try {
        const endpoint = await this.url('output');
        const response = await fetch(endpoint.url, {
          method: 'POST', cache: 'no-store',
          headers: { [TOKEN_HEADER]: endpoint.token, 'content-type': 'application/octet-stream' },
          body: bytes,
        });
        if (response.status !== 202) throw new Error(`Realtime paired speaker bridge returned HTTP ${response.status}.`);
      } finally {
        this.pendingOutputChunks -= 1;
        this.pendingOutputBytes -= bytes.length;
      }
    }).catch((reason) => {
      if (!this.stopped) this.onError(reason instanceof Error ? reason : new Error(String(reason)));
    });
  }

  async interruptOutput(): Promise<void> {
    if (this.stopped) return;
    this.outputPaused = true;
    await this.outputTail.catch(() => undefined);
    try {
      const endpoint = await this.url('output');
      const response = await fetch(endpoint.url, {
        method: 'DELETE', cache: 'no-store',
        headers: { [TOKEN_HEADER]: endpoint.token },
      });
      if (response.status !== 204) throw new Error(`Realtime paired speaker clear returned HTTP ${response.status}.`);
    } finally {
      this.outputPaused = false;
    }
  }

  stop(): void {
    this.stopped = true;
    this.outputPaused = true;
    this.inputAbort?.abort();
    this.inputAbort = null;
    for (const wake of [...this.inputIdleWaiters]) wake();
    this.inputIdleWaiters.clear();
  }
}
