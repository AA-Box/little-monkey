import { realtimeVoiceClient } from './companionClient';
import type {
  RealtimeVoiceCapabilities,
  RealtimeVoiceEvent,
  RealtimeVoiceProvider,
  RealtimeVoiceSession,
  RealtimeVoiceSessionConfig,
  RealtimeVoiceState,
} from './realtimeVoice';

interface DataChannelLike {
  readyState: string;
  send(data: string): void;
  close(): void;
  addEventListener(type: string, listener: EventListener): void;
  removeEventListener(type: string, listener: EventListener): void;
}

interface PeerLike {
  connectionState: RTCPeerConnectionState;
  iceConnectionState?: RTCIceConnectionState;
  addTrack(track: MediaStreamTrack, ...streams: MediaStream[]): RTCRtpSender;
  createDataChannel(label: string): RTCDataChannel;
  createOffer(): Promise<RTCSessionDescriptionInit>;
  setLocalDescription(description: RTCSessionDescriptionInit): Promise<void>;
  setRemoteDescription(description: RTCSessionDescriptionInit): Promise<void>;
  close(): void;
  addEventListener(type: string, listener: EventListener): void;
  removeEventListener(type: string, listener: EventListener): void;
  ontrack: ((event: RTCTrackEvent) => void) | null;
}

export interface OpenAiRealtimeEnvironment {
  createPeer(): PeerLike;
  getUserMedia(constraints: MediaStreamConstraints): Promise<MediaStream>;
  createAudio(): HTMLAudioElement;
  connectBroker: typeof realtimeVoiceClient.connect;
  disconnectBroker: typeof realtimeVoiceClient.disconnect;
}

const capabilities: RealtimeVoiceCapabilities = {
  inputAudio: true,
  outputAudio: true,
  inputTranscription: true,
  outputTranscription: true,
  serverVad: true,
  manualTurnDetection: true,
  interruption: true,
  tools: true,
};

function defaultEnvironment(): OpenAiRealtimeEnvironment {
  return {
    createPeer: () => new RTCPeerConnection(),
    getUserMedia: (constraints) => navigator.mediaDevices.getUserMedia(constraints),
    createAudio: () => new Audio(),
    connectBroker: realtimeVoiceClient.connect,
    disconnectBroker: realtimeVoiceClient.disconnect,
  };
}

function eventId(raw: Record<string, unknown>): string {
  const id = raw.event_id;
  if (typeof id === 'string' && id) return id;
  return `openai:${String(raw.type ?? 'unknown')}:${String(raw.item_id ?? raw.response_id ?? crypto.randomUUID())}`;
}

function readError(raw: Record<string, unknown>): { code: string; message: string } {
  const error = typeof raw.error === 'object' && raw.error ? raw.error as Record<string, unknown> : {};
  return {
    code: typeof error.code === 'string' ? error.code : 'provider_error',
    message: typeof error.message === 'string' ? error.message : 'The realtime provider returned an error.',
  };
}

export function normalizeOpenAiRealtimeEvent(raw: Record<string, unknown>): RealtimeVoiceEvent | null {
  const id = eventId(raw);
  switch (raw.type) {
    case 'session.created':
    case 'session.updated':
      return { type: 'connected', eventId: id };
    case 'input_audio_buffer.speech_started':
      return { type: 'speech_started', eventId: id, itemId: typeof raw.item_id === 'string' ? raw.item_id : undefined };
    case 'conversation.item.input_audio_transcription.completed':
      return {
        type: 'input_transcript', eventId: id,
        itemId: String(raw.item_id ?? id), text: String(raw.transcript ?? ''),
      };
    case 'response.created': {
      const response = typeof raw.response === 'object' && raw.response ? raw.response as Record<string, unknown> : {};
      return { type: 'response_started', eventId: id, responseId: String(response.id ?? id) };
    }
    case 'response.output_audio_transcript.delta':
      return {
        type: 'output_transcript_delta', eventId: id,
        itemId: String(raw.item_id ?? id), delta: String(raw.delta ?? ''),
      };
    case 'response.output_audio_transcript.done':
      return {
        type: 'output_transcript_done', eventId: id,
        itemId: String(raw.item_id ?? id), text: String(raw.transcript ?? ''),
      };
    case 'response.output_item.done': {
      const item = typeof raw.item === 'object' && raw.item ? raw.item as Record<string, unknown> : {};
      if (item.type !== 'function_call') return null;
      return {
        type: 'tool_call', eventId: id,
        call: {
          id: String(item.call_id ?? item.id ?? id),
          itemId: String(item.id ?? id),
          name: String(item.name ?? ''),
          arguments: String(item.arguments ?? ''),
        },
      };
    }
    case 'response.done': {
      const response = typeof raw.response === 'object' && raw.response ? raw.response as Record<string, unknown> : {};
      const usage = typeof response.usage === 'object' && response.usage ? response.usage as Record<string, unknown> : null;
      return {
        type: 'response_done', eventId: id, responseId: String(response.id ?? id),
        ...(usage ? { usage: {
          inputTokens: Number(usage.input_tokens ?? 0),
          outputTokens: Number(usage.output_tokens ?? 0),
        } } : {}),
      };
    }
    case 'error': {
      const error = readError(raw);
      return { type: 'error', eventId: id, ...error };
    }
    default:
      return null;
  }
}

class OpenAiRealtimeSession implements RealtimeVoiceSession {
  readonly capabilities = capabilities;
  state: RealtimeVoiceState = 'idle';
  private peer: PeerLike | null = null;
  private channel: DataChannelLike | null = null;
  private stream: MediaStream | null = null;
  private audio: HTMLAudioElement | null = null;
  private closed = false;
  private outputStarted = false;
  private responseActive = false;

  constructor(
    private readonly config: RealtimeVoiceSessionConfig,
    private readonly onEvent: (event: RealtimeVoiceEvent) => void,
    private readonly environment: OpenAiRealtimeEnvironment,
  ) {}

  private emit(event: RealtimeVoiceEvent): void {
    this.onEvent(event);
  }

  private readonly onMessage = (event: Event): void => {
    try {
      const data = JSON.parse(String((event as MessageEvent).data)) as Record<string, unknown>;
      if (data.type === 'input_audio_buffer.speech_started') {
        this.responseActive = false;
        this.outputStarted = false;
        this.audio?.pause();
      }
      if (data.type === 'response.created') {
        this.responseActive = true;
        this.outputStarted = false;
        void this.audio?.play().catch(() => undefined);
      }
      if (data.type === 'response.done') this.responseActive = false;
      if (data.type === 'response.output_audio.delta' && !this.outputStarted) {
        this.outputStarted = true;
        this.emit({ type: 'output_audio_started', eventId: `${eventId(data)}:first-audio` });
      }
      const normalized = normalizeOpenAiRealtimeEvent(data);
      if (normalized) this.emit(normalized);
    } catch {
      this.emit({
        type: 'error', eventId: `local:malformed:${crypto.randomUUID()}`,
        code: 'malformed_provider_event', message: 'The realtime provider sent a malformed event.',
      });
    }
  };

  private readonly onAudioPlaying = (): void => {
    // The remote MediaStream may enter `playing` before any model audio exists.
    // First-output timing is anchored to `response.output_audio.delta` instead.
  };

  private readonly onAudioWaiting = (): void => {
    if (this.closed || !this.outputStarted) return;
    this.emit({ type: 'output_underrun', eventId: `local:audio-underrun:${crypto.randomUUID()}` });
  };

  private readonly onConnectionState = (): void => {
    const state = this.peer?.connectionState;
    const ice = this.peer?.iceConnectionState;
    if (this.closed) return;
    const failed = state === 'failed' || ice === 'failed';
    const disconnected = state === 'disconnected' || ice === 'disconnected';
    if (failed || disconnected) {
      const failure = failed ? 'failed' : 'disconnected';
      this.emit({
        type: 'connection_lost', eventId: `local:connection:${crypto.randomUUID()}`,
        recoverable: disconnected && !failed, code: `webrtc_${failure}`,
      });
    }
  };

  async connect(): Promise<void> {
    if (this.state !== 'idle') throw new Error('Realtime session has already started.');
    this.state = 'connecting';
    try {
      const audioConstraints: MediaTrackConstraints = {
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      };
      if (this.config.inputDeviceId) audioConstraints.deviceId = { exact: this.config.inputDeviceId };
      this.stream = await this.environment.getUserMedia({ audio: audioConstraints, video: false });
      for (const track of this.stream.getAudioTracks()) {
        if (this.config.turnDetection === 'manual') track.enabled = false;
        track.addEventListener('ended', this.onTrackEnded, { once: true });
      }
      this.peer = this.environment.createPeer();
      this.peer.addEventListener('connectionstatechange', this.onConnectionState);
      this.peer.addEventListener('iceconnectionstatechange', this.onConnectionState);
      this.stream.getTracks().forEach((track) => this.peer!.addTrack(track, this.stream!));
      this.audio = this.environment.createAudio();
      this.audio.autoplay = true;
      this.audio.addEventListener('playing', this.onAudioPlaying);
      this.audio.addEventListener('waiting', this.onAudioWaiting);
      this.audio.addEventListener('stalled', this.onAudioWaiting);
      this.peer.ontrack = (event) => {
        if (!this.audio) return;
        this.audio.srcObject = event.streams[0] ?? new MediaStream([event.track]);
        const routed = this.audio as HTMLAudioElement & { setSinkId?: (id: string) => Promise<void> };
        if (this.config.outputDeviceId && routed.setSinkId) {
          void routed.setSinkId(this.config.outputDeviceId).catch(() => undefined);
        }
        void this.audio.play().catch(() => undefined);
      };
      this.channel = this.peer.createDataChannel('oai-events');
      this.channel.addEventListener('message', this.onMessage);
      const offer = await this.peer.createOffer();
      await this.peer.setLocalDescription(offer);
      const response = await this.environment.connectBroker({
        sessionId: this.config.sessionId,
        providerId: 'openai',
        model: this.config.model,
        voice: this.config.voice,
        turnDetection: this.config.turnDetection,
        sdp: offer.sdp ?? '',
        instructions: this.config.instructions,
        tools: this.config.tools.map((tool) => ({
          type: 'function' as const,
          name: tool.function.name,
          description: tool.function.description,
          parameters: tool.function.parameters,
        })),
      });
      if (this.closed) {
        await this.environment.disconnectBroker(this.config.sessionId).catch(() => undefined);
        return;
      }
      await this.peer.setRemoteDescription({ type: 'answer', sdp: response.sdpAnswer });
      await this.waitForDataChannel();
      this.state = 'ready';
      this.emit({ type: 'connected', eventId: `local:connected:${this.config.sessionId}` });
    } catch (error) {
      await this.close();
      throw error;
    }
  }

  private waitForDataChannel(): Promise<void> {
    if (this.channel?.readyState === 'open') return Promise.resolve();
    const channel = this.channel;
    if (!channel) return Promise.reject(new Error('Realtime data channel was not created.'));
    return new Promise((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        cleanup();
        reject(new Error('Realtime data channel did not open in time.'));
      }, 15_000);
      const onOpen = () => { cleanup(); resolve(); };
      const onClose = () => { cleanup(); reject(new Error('Realtime data channel closed before opening.')); };
      const cleanup = () => {
        window.clearTimeout(timeout);
        channel.removeEventListener('open', onOpen);
        channel.removeEventListener('close', onClose);
      };
      channel.addEventListener('open', onOpen);
      channel.addEventListener('close', onClose);
    });
  }

  private readonly onTrackEnded = (): void => {
    if (this.closed) return;
    this.emit({
      type: 'connection_lost', eventId: `local:microphone-ended:${crypto.randomUUID()}`,
      recoverable: false, code: 'microphone_revoked',
    });
    void this.close();
  };

  private send(event: Record<string, unknown>): void {
    if (!this.channel || this.channel.readyState !== 'open') {
      throw new Error('Realtime data channel is not open.');
    }
    this.channel.send(JSON.stringify({ event_id: crypto.randomUUID(), ...event }));
  }

  async interrupt(): Promise<void> {
    if (this.closed) return;
    this.send({ type: 'response.cancel' });
    this.send({ type: 'output_audio_buffer.clear' });
    this.responseActive = false;
    this.outputStarted = false;
    this.audio?.pause();
    this.emit({ type: 'interrupted', eventId: `local:interrupt:${crypto.randomUUID()}` });
  }

  async startManualTurn(): Promise<void> {
    if (this.config.turnDetection !== 'manual') return;
    let interrupted = false;
    if (this.responseActive) {
      this.send({ type: 'response.cancel' });
      interrupted = true;
    }
    if (this.outputStarted) {
      this.send({ type: 'output_audio_buffer.clear' });
      interrupted = true;
    }
    if (interrupted) {
      this.responseActive = false;
      this.outputStarted = false;
      this.audio?.pause();
      this.emit({ type: 'interrupted', eventId: `local:manual-interrupt:${crypto.randomUUID()}` });
    }
    this.send({ type: 'input_audio_buffer.clear' });
    this.stream?.getAudioTracks().forEach((track) => { track.enabled = true; });
    this.emit({ type: 'listening', eventId: `local:listening:${crypto.randomUUID()}` });
  }

  async finishManualTurn(): Promise<void> {
    if (this.config.turnDetection !== 'manual') return;
    this.stream?.getAudioTracks().forEach((track) => { track.enabled = false; });
    this.send({ type: 'input_audio_buffer.commit' });
    this.send({ type: 'response.create' });
  }

  sendToolResult(callId: string, output: string): void {
    this.send({
      type: 'conversation.item.create',
      item: { type: 'function_call_output', call_id: callId, output },
    });
  }

  requestResponse(): void {
    this.send({ type: 'response.create' });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.state = 'closed';
    this.channel?.removeEventListener('message', this.onMessage);
    this.channel?.close();
    if (this.peer) {
      this.peer.removeEventListener('connectionstatechange', this.onConnectionState);
      this.peer.removeEventListener('iceconnectionstatechange', this.onConnectionState);
      this.peer.ontrack = null;
      this.peer.close();
    }
    this.stream?.getTracks().forEach((track) => {
      track.removeEventListener('ended', this.onTrackEnded);
      track.stop();
    });
    if (this.audio) {
      this.audio.removeEventListener('playing', this.onAudioPlaying);
      this.audio.removeEventListener('waiting', this.onAudioWaiting);
      this.audio.removeEventListener('stalled', this.onAudioWaiting);
      this.audio.pause();
      this.audio.srcObject = null;
    }
    this.peer = null;
    this.channel = null;
    this.stream = null;
    this.audio = null;
    await this.environment.disconnectBroker(this.config.sessionId).catch(() => undefined);
  }
}

export class OpenAiRealtimeVoiceProvider implements RealtimeVoiceProvider {
  readonly id = 'openai';
  readonly capabilities = capabilities;

  constructor(private readonly environment: OpenAiRealtimeEnvironment = defaultEnvironment()) {}

  createSession(
    config: RealtimeVoiceSessionConfig,
    onEvent: (event: RealtimeVoiceEvent) => void,
  ): RealtimeVoiceSession {
    return new OpenAiRealtimeSession(config, onEvent, this.environment);
  }
}
