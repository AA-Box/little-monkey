import {
  OpenAiRealtimeVoiceProvider,
  type OpenAiRealtimeEnvironment,
} from './openAiRealtimeVoice';
import type {
  RealtimeVoiceEvent,
  RealtimeVoiceProvider,
  RealtimeVoiceSession,
  RealtimeVoiceSessionConfig,
} from './realtimeVoice';

/**
 * A realtime far end that runs inside this process.
 *
 * This is a TEST DOUBLE FOR OPENAI'S SERVERS AND NOTHING ELSE. Everything
 * between it and the phone — the Talk socket, the daemon, the media bridge,
 * `OpenAiRealtimeSession`, its data-channel dialect, its track handling, its
 * routing and its teardown — stays the production code. Only the far end is
 * replaced, which is what lets Voice Everywhere be proven end to end with no
 * OpenAI credential: the provider's servers are the one participant an
 * operator without a key cannot stand up, and the one participant whose
 * behaviour is not this project's code.
 *
 * It must never be the default provider. OpenAI stays the default; this exists
 * so that a routing claim can be tested, and a routing test that quietly became
 * the shipping path would be a product that talks to a tone generator.
 */

/** What the far end says back. Fixed text, because an acceptance report that
 * changed wording between runs would make a diff of two runs unreadable. */
const SPOKEN_ANSWER = 'I read the file you asked about and it is there.';
const HEARD_REQUEST = 'Please read the acceptance file and tell me what is in it.';

/** How long one spoken answer lasts. Audio has duration in the real world and
 * the session's own probe needs a window in which to observe rising energy, so
 * `response.done` is held back rather than fired in the same tick as the first
 * transcript delta — which would leave `outputStarted` true for no measurable
 * time and make first-audio evidence unobservable. */
const DEFAULT_SPEECH_MS = 2_000;

/** A tone rather than an echo of the offered track. Under the routing this
 * exists to prove, the paired device's audio arrives as `input_audio_buffer`
 * PCM on the data channel and the offered media track carries nothing, so an
 * echo would return silence and certify nothing. A generated tone is real
 * audio on the real track whichever way the microphone is routed. */
const TONE_HZ = 440;

/** Enough routed PCM to call a turn over when the host is using provider VAD.
 * A byte count rather than a silence timer: the far end cannot decode speech,
 * and a wall-clock turn boundary would make every test that drives it racy. */
const VAD_TURN_BYTES = 64_000;

type BrokerRequest = Parameters<OpenAiRealtimeEnvironment['connectBroker']>[0];

export interface LoopbackDialectConfig {
  instructions: string;
  tools: BrokerRequest['tools'];
  turnDetection: BrokerRequest['turnDetection'];
  speechMs?: number;
  /** Schedules the end of a spoken answer. Tests replace it with an immediate
   * call so the dialect can be driven without wall-clock waits. */
  defer?: (run: () => void, ms: number) => void;
  /** Gates the outbound tone, so the track carries sound only while the far
   * end is actually speaking and a barge-in is audible as a stop. */
  onSpeaking?: (speaking: boolean) => void;
}

/**
 * The data-channel half of the far end: the OpenAI Realtime event dialect,
 * with no transport and no WebRTC, so it can be driven directly.
 *
 * Deterministic by construction — every id is a counter, every utterance is a
 * constant, and the only elapsed time is the length of a spoken answer.
 */
export class LoopbackRealtimeDialect {
  private sequence = 0;
  private responseCount = 0;
  private activeResponseId: string | null = null;
  private turnOpen = false;
  private appendedBytes = 0;
  private toolCallIssued = false;
  private closed = false;

  constructor(
    private readonly transmit: (payload: string) => void,
    private readonly config: LoopbackDialectConfig,
  ) {}

  /** Called when the far end's side of the data channel opens. */
  open(): void {
    this.emit('session.created', { session: { id: `loopback_session_${this.sequence}` } });
  }

  close(): void {
    this.closed = true;
    this.activeResponseId = null;
    this.config.onSpeaking?.(false);
  }

  receive(payload: string): void {
    if (this.closed) return;
    let event: Record<string, unknown>;
    try {
      event = JSON.parse(payload) as Record<string, unknown>;
    } catch {
      // A real provider answers malformed input with an error event rather
      // than by going quiet, and a host that mishandles that should fail here.
      this.emit('error', { error: { code: 'invalid_request_error', message: 'The client sent a malformed event.' } });
      return;
    }
    switch (event.type) {
      case 'input_audio_buffer.clear':
        this.turnOpen = false;
        this.appendedBytes = 0;
        return;
      case 'input_audio_buffer.append':
        this.append(String(event.audio ?? ''));
        return;
      case 'input_audio_buffer.commit':
        this.commit();
        return;
      case 'response.create':
        this.respond();
        return;
      case 'response.cancel':
        this.cancel();
        return;
      case 'output_audio_buffer.clear':
        this.config.onSpeaking?.(false);
        return;
      default:
        return;
    }
  }

  private emit(type: string, body: Record<string, unknown> = {}): void {
    this.sequence += 1;
    this.transmit(JSON.stringify({ event_id: `loopback_${this.sequence}`, type, ...body }));
  }

  private append(audioBase64: string): void {
    if (!audioBase64) return;
    // A new turn is only opened between answers. Routed PCM keeps arriving
    // while the far end is speaking, and treating that as fresh speech would
    // raise `speech_started` against a live response — which the session reads
    // as a barge-in and would tear down every answer it ever gave.
    if (!this.turnOpen && this.activeResponseId === null) {
      this.turnOpen = true;
      this.emit('input_audio_buffer.speech_started', { item_id: this.itemId() });
    }
    if (!this.turnOpen) return;
    this.appendedBytes += audioBase64.length;
    if (this.config.turnDetection === 'semantic_vad' && this.appendedBytes >= VAD_TURN_BYTES) {
      this.commit();
      this.respond();
    }
  }

  private commit(): void {
    this.turnOpen = false;
    this.appendedBytes = 0;
    this.emit('conversation.item.input_audio_transcription.completed', {
      item_id: this.itemId(),
      transcript: HEARD_REQUEST,
    });
  }

  private respond(): void {
    if (this.activeResponseId !== null) {
      // The real provider refuses a second concurrent response, and the
      // session holds its continuation back for exactly that reason.
      this.emit('error', { error: { code: 'conversation_already_has_active_response', message: 'A response is already in progress.' } });
      return;
    }
    this.responseCount += 1;
    const responseId = `loopback_resp_${this.responseCount}`;
    this.activeResponseId = responseId;
    this.emit('response.created', { response: { id: responseId } });

    const tool = this.config.tools[0];
    if (tool && !this.toolCallIssued) {
      // The first answer asks for the host tool; the continuation the host then
      // requests is the one that speaks. That is the shape the response ledger
      // and the acceptance run are written against.
      this.toolCallIssued = true;
      this.emit('response.output_item.done', {
        response_id: responseId,
        item: {
          id: `${responseId}_item`,
          type: 'function_call',
          call_id: `${responseId}_call`,
          name: tool.name,
          arguments: this.toolArguments(tool),
        },
      });
      this.finish(responseId, 'completed');
      return;
    }

    const itemId = `${responseId}_item`;
    this.config.onSpeaking?.(true);
    this.emit('response.output_audio_transcript.delta', { item_id: itemId, delta: SPOKEN_ANSWER });
    const speak = this.config.defer ?? ((run, ms) => { setTimeout(run, ms); });
    speak(() => {
      if (this.closed || this.activeResponseId !== responseId) return;
      this.emit('response.output_audio_transcript.done', { item_id: itemId, transcript: SPOKEN_ANSWER });
      this.finish(responseId, 'completed');
    }, this.config.speechMs ?? DEFAULT_SPEECH_MS);
  }

  private cancel(): void {
    const responseId = this.activeResponseId;
    this.config.onSpeaking?.(false);
    if (responseId === null) return;
    this.finish(responseId, 'cancelled');
  }

  private finish(responseId: string, status: 'completed' | 'cancelled'): void {
    this.activeResponseId = null;
    // The utterance is over either way, so the track goes quiet. A tone that
    // kept sounding after `response.done` would let a barge-in that never
    // stopped anything still look like silence had fallen.
    this.config.onSpeaking?.(false);
    this.emit('response.done', { response: { id: responseId, status } });
  }

  private itemId(): string {
    return `loopback_item_${this.responseCount}_${this.sequence}`;
  }

  /** The far end has no model, so it follows the session instructions the only
   * way a double can: the caller names the argument as a JSON string literal in
   * the instructions, and the tool's own schema names the field it belongs in. */
  private toolArguments(tool: BrokerRequest['tools'][number]): string {
    const literal = /"(?:[^"\\]|\\.)*"/.exec(this.config.instructions)?.[0];
    const parameters = tool.parameters as { required?: unknown } | null;
    const required = Array.isArray(parameters?.required) ? parameters.required : [];
    const field = typeof required[0] === 'string' ? required[0] : null;
    if (literal === undefined || field === null) return '{}';
    return JSON.stringify({ [field]: JSON.parse(literal) as string });
  }
}

/** The answering peer: a real `RTCPeerConnection` in this process that
 * completes a real SDP exchange with the offer the production session made,
 * returns real audio on a real track, and speaks the dialect over the real
 * data channel the session opened. */
class LoopbackFarEnd {
  private readonly dialect: LoopbackRealtimeDialect;
  private channel: RTCDataChannel | null = null;
  private context: AudioContext | null = null;
  private oscillator: OscillatorNode | null = null;
  private gain: GainNode | null = null;
  private readonly toNear: RTCIceCandidateInit[] = [];
  private readonly onNearSignalingState = (): void => { this.flushToNear(); };

  constructor(
    private readonly near: RTCPeerConnection,
    private readonly peer: RTCPeerConnection,
    private readonly createAudioContext: () => AudioContext,
    request: BrokerRequest,
    options: Pick<LoopbackRealtimeOptions, 'speechMs' | 'defer'>,
  ) {
    this.dialect = new LoopbackRealtimeDialect((payload) => {
      if (this.channel?.readyState === 'open') this.channel.send(payload);
    }, {
      instructions: request.instructions,
      tools: request.tools,
      turnDetection: request.turnDetection,
      speechMs: options.speechMs,
      defer: options.defer,
      onSpeaking: (speaking) => { if (this.gain) this.gain.gain.value = speaking ? 1 : 0; },
    });
    this.peer.addEventListener('icecandidate', (event) => {
      const candidate = (event as RTCPeerConnectionIceEvent).candidate;
      if (candidate) {
        this.toNear.push(candidate.toJSON());
        this.flushToNear();
      }
    });
    // The near peer cannot accept a candidate before our answer is applied, and
    // applying it is exactly what moves it to `stable`.
    this.near.addEventListener('signalingstatechange', this.onNearSignalingState);
    this.peer.addEventListener('datachannel', (event) => {
      this.attachChannel((event as RTCDataChannelEvent).channel);
    });
  }

  async answer(offerSdp: string): Promise<string> {
    await this.peer.setRemoteDescription({ type: 'offer', sdp: offerSdp });
    this.attachTone();
    const answer = await this.peer.createAnswer();
    await this.peer.setLocalDescription(answer);
    return this.peer.localDescription?.sdp ?? answer.sdp ?? '';
  }

  async addNearCandidate(candidate: RTCIceCandidateInit): Promise<void> {
    await this.peer.addIceCandidate(candidate).catch(() => undefined);
  }

  close(): void {
    this.dialect.close();
    this.near.removeEventListener('signalingstatechange', this.onNearSignalingState);
    this.channel?.close();
    this.channel = null;
    this.oscillator?.stop();
    this.oscillator?.disconnect();
    this.gain?.disconnect();
    this.oscillator = null;
    this.gain = null;
    const context = this.context;
    this.context = null;
    if (context && context.state !== 'closed') void context.close().catch(() => undefined);
    this.peer.close();
  }

  private flushToNear(): void {
    if (this.near.signalingState !== 'stable') return;
    for (const candidate of this.toNear.splice(0)) {
      void this.near.addIceCandidate(candidate).catch(() => undefined);
    }
  }

  private attachChannel(channel: RTCDataChannel): void {
    this.channel = channel;
    channel.addEventListener('message', (event) => {
      this.dialect.receive(String((event as MessageEvent).data));
    });
    if (channel.readyState === 'open') this.dialect.open();
    else channel.addEventListener('open', () => this.dialect.open());
  }

  private attachTone(): void {
    const context = this.createAudioContext();
    const oscillator = context.createOscillator();
    oscillator.frequency.value = TONE_HZ;
    const gain = context.createGain();
    gain.gain.value = 0;
    const destination = context.createMediaStreamDestination();
    oscillator.connect(gain);
    gain.connect(destination);
    oscillator.start();
    this.context = context;
    this.oscillator = oscillator;
    this.gain = gain;
    const track = destination.stream.getAudioTracks()[0];
    if (!track) return;
    // The offer carries a `sendrecv` audio transceiver, so the answering side
    // already has the matching sender; filling it keeps the answer to one m=
    // line and avoids a renegotiation the production session never expects.
    const transceiver = this.peer.getTransceivers().find((candidate) => candidate.receiver.track.kind === 'audio');
    if (transceiver) void transceiver.sender.replaceTrack(track).catch(() => undefined);
    else this.peer.addTrack(track, destination.stream);
  }
}

export interface LoopbackRealtimeOptions
  extends Partial<Omit<OpenAiRealtimeEnvironment, 'createPeer' | 'connectBroker' | 'disconnectBroker'>> {
  /** Builds both peers. The default is the platform's own `RTCPeerConnection`,
   * which is what makes the exchange below a real one. */
  createPeerConnection?: () => RTCPeerConnection;
  speechMs?: number;
  defer?: (run: () => void, ms: number) => void;
}

/**
 * Runs the production `OpenAiRealtimeSession` against the local far end above.
 *
 * NEVER the default provider — see the header. It is selectable so that Voice
 * Everywhere's routing can be proven without an OpenAI credential, and for no
 * other purpose.
 */
export class LoopbackRealtimeVoiceProvider implements RealtimeVoiceProvider {
  readonly id = 'loopback';
  /** Mirrored from the production provider rather than restated: the session
   * under the loopback IS the production session, so it can do nothing more
   * and nothing less, and a drifting copy here would advertise a lie. */
  readonly capabilities = new OpenAiRealtimeVoiceProvider().capabilities;

  constructor(private readonly options: LoopbackRealtimeOptions = {}) {}

  createSession(
    config: RealtimeVoiceSessionConfig,
    onEvent: (event: RealtimeVoiceEvent) => void,
  ): RealtimeVoiceSession {
    // A fresh environment per session: it holds this conversation's two peers.
    return new OpenAiRealtimeVoiceProvider(this.environment()).createSession(config, onEvent);
  }

  private environment(): OpenAiRealtimeEnvironment {
    const createPeerConnection = this.options.createPeerConnection ?? (() => new RTCPeerConnection());
    const createAudioContext = this.options.createAudioContext ?? (() => new AudioContext());
    let near: RTCPeerConnection | null = null;
    let far: LoopbackFarEnd | null = null;
    // Candidates the near peer gathers before the far end exists. It starts
    // gathering the moment it has a local description, which is before the
    // broker call, so dropping these would lose the only route on some hosts.
    const pending: RTCIceCandidateInit[] = [];
    return {
      createPeer: () => {
        const peer = createPeerConnection();
        near = peer;
        peer.addEventListener('icecandidate', (event) => {
          const candidate = (event as RTCPeerConnectionIceEvent).candidate;
          if (!candidate) return;
          if (far) void far.addNearCandidate(candidate.toJSON());
          else pending.push(candidate.toJSON());
        });
        return peer;
      },
      getUserMedia: this.options.getUserMedia
        ?? ((constraints) => navigator.mediaDevices.getUserMedia(constraints)),
      createAudio: this.options.createAudio ?? (() => new Audio()),
      createAudioContext,
      connectBroker: async (request) => {
        const nearPeer = near;
        if (!nearPeer) throw new Error('The loopback far end was asked to answer before a peer existed.');
        const end = new LoopbackFarEnd(nearPeer, createPeerConnection(), createAudioContext, request, this.options);
        const sdpAnswer = await end.answer(request.sdp);
        far = end;
        for (const candidate of pending.splice(0)) void end.addNearCandidate(candidate);
        return { sessionId: request.sessionId, sdpAnswer, providerRequestId: null };
      },
      disconnectBroker: async () => {
        far?.close();
        far = null;
        near = null;
      },
      ...(this.options.audioProbeIntervalMs === undefined
        ? {}
        : { audioProbeIntervalMs: this.options.audioProbeIntervalMs }),
    };
  }
}
