import { textContent, type ChatMessage, type ToolDef } from './llamaClient';

export function boundedRealtimeContext(
  messages: readonly ChatMessage[],
  maxMessages = 12,
  maxBytes = 8_000,
): string {
  if (maxMessages <= 0 || maxBytes <= 0) return '';
  const lines = messages
    .filter((message) => message.role === 'user' || message.role === 'assistant')
    .slice(-Math.floor(maxMessages))
    .map((message) => `${message.role === 'user' ? 'User' : 'Assistant'}: ${textContent(message.content).trim()}`)
    .filter((line) => !line.endsWith(':'));
  let result = lines.join('\n');
  while (new TextEncoder().encode(result).byteLength > maxBytes && lines.length > 1) {
    lines.shift();
    result = lines.join('\n');
  }
  if (new TextEncoder().encode(result).byteLength <= maxBytes) return result;
  let bounded = '';
  for (const point of result) {
    if (new TextEncoder().encode(`${bounded}${point}`).byteLength > maxBytes) break;
    bounded += point;
  }
  return bounded;
}

export type RealtimeVoiceState =
  | 'idle'
  | 'connecting'
  | 'ready'
  | 'listening'
  | 'responding'
  | 'awaiting_approval'
  | 'reconnecting'
  | 'error'
  | 'closed';

export interface RealtimeVoiceCapabilities {
  inputAudio: boolean;
  outputAudio: boolean;
  inputTranscription: boolean;
  outputTranscription: boolean;
  serverVad: boolean;
  manualTurnDetection: boolean;
  interruption: boolean;
  tools: boolean;
}

export interface RealtimeVoiceToolCall {
  id: string;
  name: string;
  arguments: string;
  itemId: string;
}

export type RealtimeVoiceEvent =
  | { type: 'connected'; eventId: string }
  | { type: 'listening'; eventId: string }
  | { type: 'speech_started'; eventId: string; itemId?: string }
  | { type: 'input_transcript'; eventId: string; itemId: string; text: string }
  | { type: 'response_started'; eventId: string; responseId: string }
  | { type: 'output_audio_started'; eventId: string }
  | { type: 'output_underrun'; eventId: string }
  | { type: 'output_transcript_delta'; eventId: string; itemId: string; delta: string }
  | { type: 'output_transcript_done'; eventId: string; itemId: string; text: string }
  | { type: 'tool_call'; eventId: string; call: RealtimeVoiceToolCall }
  | { type: 'response_done'; eventId: string; responseId: string; usage?: { inputTokens: number; outputTokens: number } }
  | { type: 'interrupted'; eventId: string; itemId?: string }
  | { type: 'connection_lost'; eventId: string; recoverable: boolean; code: string }
  | { type: 'error'; eventId: string; code: string; message: string };

export interface RealtimeVoiceMetrics {
  connectionMs: number | null;
  firstRecognizedSpeechMs: number | null;
  firstModelEventMs: number | null;
  firstAudioMs: number | null;
  endToEndMs: number | null;
  interrupted: boolean;
  reconnectCount: number;
  errorCode: string | null;
  toolRoundTripMs: number[];
  outputUnderruns: number;
  inputTokens: number;
  outputTokens: number;
}

export interface RealtimeVoiceSessionConfig {
  sessionId: string;
  model: string;
  voice: string;
  turnDetection: 'semantic_vad' | 'manual';
  inputDeviceId: string | null;
  outputDeviceId: string | null;
  instructions: string;
  tools: ToolDef[];
}

export interface RealtimeVoiceSession {
  readonly capabilities: RealtimeVoiceCapabilities;
  readonly state: RealtimeVoiceState;
  connect(): Promise<void>;
  interrupt(): Promise<void>;
  startManualTurn(): Promise<void>;
  finishManualTurn(): Promise<void>;
  sendToolResult(callId: string, output: string): void;
  requestResponse(): void;
  close(): Promise<void>;
}

export interface RealtimeVoiceProvider {
  readonly id: string;
  readonly capabilities: RealtimeVoiceCapabilities;
  createSession(
    config: RealtimeVoiceSessionConfig,
    onEvent: (event: RealtimeVoiceEvent) => void,
  ): RealtimeVoiceSession;
}

/** Pure state reducer shared by the UI and contract tests. Event ids are
 * bounded and deduplicated so a reconnect cannot replay transcript/tool work. */
export class RealtimeVoiceController {
  state: RealtimeVoiceState = 'idle';
  readonly metrics: RealtimeVoiceMetrics = {
    connectionMs: null,
    firstRecognizedSpeechMs: null,
    firstModelEventMs: null,
    firstAudioMs: null,
    endToEndMs: null,
    interrupted: false,
    reconnectCount: 0,
    errorCode: null,
    toolRoundTripMs: [],
    outputUnderruns: 0,
    inputTokens: 0,
    outputTokens: 0,
  };
  private readonly seen = new Set<string>();
  private readonly seenOrder: string[] = [];
  private startedAt = 0;
  private turnStartedAt = 0;

  reset(): void {
    Object.assign(this.metrics, {
      connectionMs: null,
      firstRecognizedSpeechMs: null,
      firstModelEventMs: null,
      firstAudioMs: null,
      endToEndMs: null,
      interrupted: false,
      reconnectCount: 0,
      errorCode: null,
      toolRoundTripMs: [],
      outputUnderruns: 0,
      inputTokens: 0,
      outputTokens: 0,
    } satisfies RealtimeVoiceMetrics);
    this.seen.clear();
    this.seenOrder.length = 0;
    this.startedAt = 0;
    this.turnStartedAt = 0;
    this.state = 'idle';
  }

  connecting(reconnect = false): void {
    this.startedAt = performance.now();
    this.state = reconnect ? 'reconnecting' : 'connecting';
    if (reconnect) this.metrics.reconnectCount += 1;
  }

  awaitingApproval(): void {
    this.state = 'awaiting_approval';
  }

  consume(event: RealtimeVoiceEvent): boolean {
    if (this.seen.has(event.eventId)) return false;
    this.seen.add(event.eventId);
    this.seenOrder.push(event.eventId);
    if (this.seenOrder.length > 2_048) {
      this.seen.delete(this.seenOrder.shift()!);
    }
    switch (event.type) {
      case 'connected':
        this.metrics.connectionMs = Math.max(0, Math.round(performance.now() - this.startedAt));
        this.state = 'ready';
        break;
      case 'listening':
        this.turnStartedAt = performance.now();
        this.state = 'listening';
        break;
      case 'speech_started':
        if (this.metrics.firstRecognizedSpeechMs === null && this.startedAt > 0) {
          this.metrics.firstRecognizedSpeechMs = Math.max(0, Math.round(performance.now() - this.startedAt));
        }
        this.turnStartedAt = performance.now();
        this.state = 'listening';
        break;
      case 'response_started':
        if (this.metrics.firstModelEventMs === null && this.turnStartedAt > 0) {
          this.metrics.firstModelEventMs = Math.max(0, Math.round(performance.now() - this.turnStartedAt));
        }
        this.state = 'responding';
        break;
      case 'output_audio_started':
        if (this.metrics.firstAudioMs === null && this.turnStartedAt > 0) {
          this.metrics.firstAudioMs = Math.max(0, Math.round(performance.now() - this.turnStartedAt));
        }
        this.state = 'responding';
        break;
      case 'output_underrun':
        this.metrics.outputUnderruns += 1;
        break;
      case 'output_transcript_delta':
        this.state = 'responding';
        break;
      case 'tool_call':
        this.state = 'awaiting_approval';
        break;
      case 'response_done':
        this.metrics.inputTokens += Math.max(0, Math.round(event.usage?.inputTokens ?? 0));
        this.metrics.outputTokens += Math.max(0, Math.round(event.usage?.outputTokens ?? 0));
        if (this.turnStartedAt > 0) {
          this.metrics.endToEndMs = Math.max(0, Math.round(performance.now() - this.turnStartedAt));
        }
        this.state = 'ready';
        break;
      case 'interrupted':
        this.metrics.interrupted = true;
        this.state = 'listening';
        break;
      case 'connection_lost':
        this.metrics.errorCode = event.code;
        this.state = event.recoverable ? 'reconnecting' : 'error';
        break;
      case 'error':
        this.metrics.errorCode = event.code;
        this.state = 'error';
        break;
      case 'input_transcript':
      case 'output_transcript_done':
        break;
    }
    return true;
  }

  close(): void {
    this.state = 'closed';
  }

  recordToolRoundTrip(durationMs: number): void {
    if (this.metrics.toolRoundTripMs.length >= 64) this.metrics.toolRoundTripMs.shift();
    this.metrics.toolRoundTripMs.push(Math.max(0, Math.round(durationMs)));
  }
}
