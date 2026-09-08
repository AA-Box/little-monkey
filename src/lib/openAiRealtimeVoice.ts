import { realtimeVoiceClient } from './companionClient';
import type {
  RealtimeAudioProgress,
  RealtimeResponseStatus,
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
  /** How often played-out audio is sampled once the remote track arrives.
   * Only the tests set it; the default is fine for a metric and a barge-in
   * check, and a tighter loop would burn battery for nothing. */
  audioProbeIntervalMs?: number;
}

/** The half of `RTCRtpReceiver` this adapter needs. Kept structural so a test
 * can supply statistics without standing up a peer connection. */
interface AudioReceiverLike {
  getStats(): Promise<RTCStatsReport>;
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
  // `response.output_item.done` carries no `item_id`, so two parallel function
  // calls in one response would otherwise share a fallback id and the second
  // would be deduplicated away — never executed, never answered.
  const item = typeof raw.item === 'object' && raw.item ? raw.item as Record<string, unknown> : {};
  const discriminator = raw.item_id ?? item.id ?? item.call_id ?? raw.response_id ?? crypto.randomUUID();
  return `openai:${String(raw.type ?? 'unknown')}:${String(discriminator)}`;
}

const RESPONSE_STATUSES: readonly RealtimeResponseStatus[] = ['completed', 'cancelled', 'incomplete', 'failed'];

/** A response without a recognizable status is treated as completed, which is
 * the only reading that still produces the spoken answer a tool result owes
 * the operator. Cancellation and failure are the states that must be explicit. */
function readResponseStatus(value: unknown): RealtimeResponseStatus {
  return RESPONSE_STATUSES.find((status) => status === value) ?? 'completed';
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
        responseId: typeof raw.response_id === 'string' && raw.response_id ? raw.response_id : null,
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
        status: readResponseStatus(response.status),
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
  private receiver: AudioReceiverLike | null = null;
  private audioProbe: ReturnType<typeof setInterval> | null = null;
  private lastProgress: RealtimeAudioProgress | null = null;
  private audioConfirmed = false;
  private playbackStarted = false;
  private closed = false;
  private outputStarted = false;
  private responseActive = false;
  /** A continuation asked for while the provider still had a response in
   * flight. The provider rejects a second concurrent response, so the ask is
   * held and sent when the active one finishes. */
  private pendingResponseRequest = false;

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
        // Provider VAD interrupting an answer in progress is a barge-in, and
        // the interruption metric has to count it the same as a local Stop.
        // Read before the flags are cleared, or the condition is always false.
        const bargedIn = this.responseActive || this.outputStarted;
        this.responseActive = false;
        this.outputStarted = false;
        this.audioConfirmed = false;
        this.pendingResponseRequest = false;
        this.audio?.pause();
        if (bargedIn) {
          this.emit({ type: 'interrupted', eventId: `${eventId(data)}:barge-in` });
        }
      }
      if (data.type === 'response.created') {
        this.responseActive = true;
        this.outputStarted = false;
        this.audioConfirmed = false;
        void this.audio?.play().then(() => { this.playbackStarted = true; }).catch(() => undefined);
      }
      if (data.type === 'response.done') {
        this.responseActive = false;
        // Left set, this makes the next manual turn report an interruption that
        // never happened, which in turn marks a live response non-continuable
        // and silently drops the answer a tool result was owed.
        this.outputStarted = false;
        this.flushPendingResponseRequest();
      }
      // Over WebRTC the spoken audio arrives on the media track, so a session
      // may never see `response.output_audio.delta` on the data channel — that
      // event belongs to the WebSocket transport. The transcript deltas are
      // emitted for both transports and accompany the audio being generated,
      // so whichever arrives first anchors "the model started speaking".
      // Without this, first-audio latency and every barge-in guard would be
      // dead code on the transport this adapter actually uses.
      const generating = data.type === 'response.output_audio.delta'
        || data.type === 'response.output_audio_transcript.delta';
      if (generating && !this.outputStarted) {
        this.outputStarted = true;
        this.emit({ type: 'output_generation_started', eventId: `${eventId(data)}:first-output` });
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
        void this.audio.play().then(() => { this.playbackStarted = true; }).catch(() => undefined);
        // This is the only path model audio takes over WebRTC, so its arrival
        // is reported on its own and playback is measured from here on.
        const receiver = (event as RTCTrackEvent & { receiver?: AudioReceiverLike }).receiver ?? null;
        if (receiver) this.receiver = receiver;
        this.emit({ type: 'remote_audio_track', eventId: `local:remote-track:${this.config.sessionId}` });
        this.startAudioProbe();
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

  /** Reads inbound audio statistics and the element's playback position. Null
   * when the transport gave us no receiver to ask, which a caller must treat as
   * "cannot verify" rather than as silence. */
  async audioProgress(): Promise<RealtimeAudioProgress | null> {
    const receiver = this.receiver;
    if (!receiver) return null;
    const report = await receiver.getStats().catch(() => null);
    if (!report) return null;
    let bytesReceived = 0;
    let samplesReceived = 0;
    let audioEnergy = 0;
    let audioEnergyReported = false;
    report.forEach((entry) => {
      const stat = entry as Record<string, unknown>;
      if (stat.type !== 'inbound-rtp') return;
      if (typeof stat.kind === 'string' && stat.kind !== 'audio') return;
      bytesReceived += Number(stat.bytesReceived ?? 0);
      if (typeof stat.totalSamplesReceived === 'number') samplesReceived += stat.totalSamplesReceived;
      if (typeof stat.totalAudioEnergy === 'number') {
        audioEnergy += stat.totalAudioEnergy;
        audioEnergyReported = true;
      }
    });
    return {
      bytesReceived, samplesReceived, audioEnergy, audioEnergyReported,
      playbackSeconds: this.audio?.currentTime ?? 0,
      playbackPaused: this.audio?.paused ?? true,
      playbackStarted: this.playbackStarted,
    };
  }

  private startAudioProbe(): void {
    if (this.audioProbe !== null || this.closed) return;
    const interval = this.environment.audioProbeIntervalMs ?? 250;
    this.audioProbe = setInterval(() => { void this.probeAudioOnce(); }, interval);
  }

  private async probeAudioOnce(): Promise<void> {
    if (this.closed) return;
    const progress = await this.audioProgress();
    if (!progress) return;
    const previous = this.lastProgress;
    this.lastProgress = progress;
    if (!previous || this.audioConfirmed || !this.outputStarted) return;
    // Energy, and only energy. Bytes and packets keep flowing for a track
    // carrying silence, and `totalSamplesReceived` counts samples whether or
    // not they hold any signal — accepting it as a fallback would let a silent
    // stream certify itself as audible on exactly the webviews that do not
    // report energy. A webview that reports none is reported as unable to
    // prove this, which is a different answer from "silent".
    // Only the current reading has to report energy: the figure is cumulative
    // from zero, so a previous reading that lacked the field contributes a zero
    // baseline and any positive energy is still a real increase. A silent
    // stream holds at zero and can never satisfy this.
    const nonSilent = progress.audioEnergyReported && progress.audioEnergy > previous.audioEnergy;
    if (!nonSilent) return;
    this.audioConfirmed = true;
    this.emit({
      type: 'non_silent_remote_audio', eventId: `local:non-silent-audio:${crypto.randomUUID()}`, progress,
    });
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
    this.audioConfirmed = false;
    this.pendingResponseRequest = false;
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
      this.audioConfirmed = false;
      this.pendingResponseRequest = false;
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
    // With semantic VAD the provider can open its own response while a host
    // tool is still running, so a tool result can land mid-response. Asking
    // then is an error the provider refuses outright and the spoken follow-up
    // would be lost, so the ask waits for `response.done`.
    if (this.responseActive) {
      this.pendingResponseRequest = true;
      return;
    }
    this.send({ type: 'response.create' });
  }

  private flushPendingResponseRequest(): void {
    if (!this.pendingResponseRequest || this.closed) return;
    this.pendingResponseRequest = false;
    try {
      this.send({ type: 'response.create' });
    } catch {
      // The channel closed under us; the session-level teardown reports it.
    }
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.state = 'closed';
    if (this.audioProbe !== null) {
      clearInterval(this.audioProbe);
      this.audioProbe = null;
    }
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
    this.receiver = null;
    this.lastProgress = null;
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
