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

/** Terminal status a provider reports for one response. A cancelled or failed
 * response is never continued: the spoken follow-up it would have produced was
 * abandoned by a barge-in or by the provider itself. */
export type RealtimeResponseStatus = 'completed' | 'cancelled' | 'incomplete' | 'failed';

/** What must happen once a response finishes. `continue` means every function
 * call it requested is already answered and the provider must be asked for the
 * spoken follow-up now; `await_tools` means the last settling call will ask;
 * `complete` means an ordinary finished turn. */
export type RealtimeResponseDisposition = 'continue' | 'await_tools' | 'complete';

/** What answering one function call means for the turn. `continue` asks the
 * provider for the spoken follow-up now; `wait` leaves that to `response.done`
 * or to a sibling call still running; `closed` means nothing further will
 * arrive for this response — it was cancelled, failed, or abandoned — so the
 * turn has to be closed out by the caller. */
export type RealtimeToolSettlement = 'continue' | 'wait' | 'closed';

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
  | { type: 'tool_call'; eventId: string; responseId: string | null; call: RealtimeVoiceToolCall }
  | {
    type: 'response_done'; eventId: string; responseId: string;
    status: RealtimeResponseStatus;
    usage?: { inputTokens: number; outputTokens: number };
  }
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

interface ResponseRecord {
  calls: Set<string>;
  settled: Set<string>;
  finished: boolean;
  continuable: boolean;
  resolved: boolean;
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
  /** One entry per provider response that is still interesting: which function
   * calls it asked for, which of them are answered, and whether it finished.
   * A response that requested tools is only complete once the provider has
   * been asked for the spoken follow-up, and that ask must happen exactly once
   * whichever arrives last — `response.done` or the final tool result. */
  private readonly responses = new Map<string, ResponseRecord>();
  private readonly callOwners = new Map<string, string>();
  private lastResponseId: string | null = null;
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
    this.responses.clear();
    this.callOwners.clear();
    this.lastResponseId = null;
    this.startedAt = 0;
    this.turnStartedAt = 0;
    this.state = 'idle';
  }

  private record(responseId: string): ResponseRecord {
    const existing = this.responses.get(responseId);
    if (existing) return existing;
    // A long session must not accumulate one entry per response forever. The
    // oldest is dropped first; 64 is far more than the one or two responses a
    // single spoken turn can have in flight.
    if (this.responses.size >= 64) {
      const oldest = this.responses.keys().next().value;
      if (oldest !== undefined) this.forget(oldest);
    }
    const created: ResponseRecord = {
      calls: new Set(), settled: new Set(), finished: false, continuable: true, resolved: false,
    };
    this.responses.set(responseId, created);
    return created;
  }

  private forget(responseId: string): void {
    const record = this.responses.get(responseId);
    if (record) {
      for (const callId of record.calls) this.callOwners.delete(callId);
    }
    this.responses.delete(responseId);
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
        this.lastResponseId = event.responseId;
        this.record(event.responseId);
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
      case 'tool_call': {
        // `response_id` is part of the provider event; the last started
        // response is only a fallback so an unattributed call still belongs to
        // a response that can be continued.
        const owner = event.responseId ?? this.lastResponseId ?? `call:${event.call.id}`;
        this.record(owner).calls.add(event.call.id);
        this.callOwners.set(event.call.id, owner);
        this.state = 'awaiting_approval';
        break;
      }
      case 'response_done': {
        this.metrics.inputTokens += Math.max(0, Math.round(event.usage?.inputTokens ?? 0));
        this.metrics.outputTokens += Math.max(0, Math.round(event.usage?.outputTokens ?? 0));
        if (this.turnStartedAt > 0) {
          this.metrics.endToEndMs = Math.max(0, Math.round(performance.now() - this.turnStartedAt));
        }
        const record = this.record(event.responseId);
        record.finished = true;
        if (event.status === 'cancelled' || event.status === 'failed') record.continuable = false;
        // OpenAI ends a response as soon as it emits a function call, so this
        // arrives while the host is still executing the tool — reporting
        // `ready` there would tell the operator the turn is over. And a
        // barge-in leaves them speaking, so `ready` would also overwrite the
        // listening state the interruption just established.
        const owed = record.settled.size < record.calls.size;
        if (event.status !== 'cancelled' && !owed) this.state = 'ready';
        break;
      }
      case 'interrupted':
        this.metrics.interrupted = true;
        // Whatever the abandoned response asked for must not produce a spoken
        // follow-up once its tool results arrive late.
        for (const record of this.responses.values()) {
          if (!record.resolved) record.continuable = false;
        }
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
    this.responses.clear();
    this.callOwners.clear();
    this.lastResponseId = null;
    this.state = 'closed';
  }

  /** How a just-finished response must be handled. Call this after
   * `consume()` has taken the matching `response_done`. */
  responseDisposition(responseId: string): RealtimeResponseDisposition {
    const record = this.responses.get(responseId);
    if (!record || record.resolved) return 'complete';
    if (record.calls.size === 0 || !record.continuable) {
      record.resolved = true;
      this.forget(responseId);
      return 'complete';
    }
    if (record.settled.size < record.calls.size) return 'await_tools';
    record.resolved = true;
    this.forget(responseId);
    return 'continue';
  }

  /** Records that one function call has been answered, and says what that
   * means for the turn. `continue` is returned exactly once per response — for
   * the call that settles the last outstanding one of a finished, continuable
   * response. */
  settleToolCall(callId: string): RealtimeToolSettlement {
    const owner = this.callOwners.get(callId);
    // Either the response already reached a terminal decision and forgot this
    // call, or the call was never tracked. Nothing else will arrive for it.
    if (owner === undefined) return 'closed';
    const record = this.responses.get(owner);
    if (!record || record.resolved) return 'closed';
    record.settled.add(callId);
    if (!record.continuable) {
      // Cancelled, failed, or abandoned by a barge-in. Only once every call
      // has been answered is the response finished with.
      if (record.settled.size < record.calls.size) return 'wait';
      record.resolved = true;
      this.forget(owner);
      return 'closed';
    }
    if (!record.finished) return 'wait';
    if (record.settled.size < record.calls.size) return 'wait';
    record.resolved = true;
    this.forget(owner);
    return 'continue';
  }

  /** True while any tracked response still has an unanswered function call.
   * Used to keep a turn from being finalized mid-tool. */
  hasOutstandingToolCalls(): boolean {
    for (const record of this.responses.values()) {
      if (!record.resolved && record.settled.size < record.calls.size) return true;
    }
    return false;
  }

  recordToolRoundTrip(durationMs: number): void {
    if (this.metrics.toolRoundTripMs.length >= 64) this.metrics.toolRoundTripMs.shift();
    this.metrics.toolRoundTripMs.push(Math.max(0, Math.round(durationMs)));
  }
}
