import { afterEach, describe, expect, it, vi } from 'vitest';

import { LoopbackRealtimeDialect, LoopbackRealtimeVoiceProvider } from './loopbackRealtimeVoice';
import { RealtimeVoiceController, type RealtimeVoiceEvent } from './realtimeVoice';

/**
 * What is and is not covered here.
 *
 * Covered: the far end's event dialect, and the wiring that carries it — the
 * production `OpenAiRealtimeSession` drives a complete tool-and-speech turn
 * against `LoopbackRealtimeVoiceProvider` with no broker, no key and no
 * network, through the real session, the real response ledger and the real
 * data-channel contract.
 *
 * NOT covered: the SDP, ICE and media-track layer. vitest runs in jsdom, which
 * has no `RTCPeerConnection`, so the peers below are in-memory stand-ins that
 * route an offer to an answer and a data channel to its twin. Proving that a
 * real offer produces a real answer and that real audio flows back on a real
 * track is the browser-run acceptance's job, not this file's. No assertion here
 * should be read as evidence about real WebRTC. A pure-TypeScript peer
 * implementation (`werift`) would close that gap in Node, but it is a new
 * dependency for a layer the browser acceptance already exercises for real, so
 * it is deliberately not added.
 */

const TOOLS = [{
  type: 'function' as const,
  name: 'read_file',
  description: 'read a file',
  parameters: { type: 'object', properties: { path: { type: 'string' } }, required: ['path'] },
}];

const INSTRUCTIONS = 'After the user speaks, call read_file exactly once with path "docs/acceptance.md".';

type Emitted = { type: string; [key: string]: unknown };

function dialect(overrides: Partial<ConstructorParameters<typeof LoopbackRealtimeDialect>[1]> = {}) {
  const emitted: Emitted[] = [];
  const speaking: boolean[] = [];
  const peer = new LoopbackRealtimeDialect((payload) => { emitted.push(JSON.parse(payload) as Emitted); }, {
    instructions: INSTRUCTIONS,
    tools: TOOLS,
    turnDetection: 'manual',
    // Runs the end of a spoken answer immediately, so the dialect can be
    // asserted without waiting out a real utterance.
    defer: (run) => { run(); },
    onSpeaking: (value) => { speaking.push(value); },
    ...overrides,
  });
  const send = (event: Record<string, unknown>) => peer.receive(JSON.stringify(event));
  return { peer, emitted, speaking, send, types: () => emitted.map((event) => event.type) };
}

describe('LoopbackRealtimeDialect', () => {
  it('answers a committed manual turn with a transcript, one tool call, and a spoken continuation', () => {
    /** Would catch a far end that never issues the function call, or that
     * speaks on the first response instead of on the continuation the host
     * requests after the tool result — either of which leaves the response
     * ledger waiting forever for an answer that never arrives. */
    const { peer, emitted, send, types } = dialect();
    peer.open();
    send({ type: 'input_audio_buffer.clear' });
    send({ type: 'input_audio_buffer.commit' });
    send({ type: 'response.create' });

    expect(types()).toEqual([
      'session.created',
      'conversation.item.input_audio_transcription.completed',
      'response.created',
      'response.output_item.done',
      'response.done',
    ]);
    const call = emitted[3].item as Record<string, unknown>;
    expect(call.type).toBe('function_call');
    expect(call.name).toBe('read_file');
    expect(call.arguments).toBe('{"path":"docs/acceptance.md"}');
    expect((emitted[4].response as Record<string, unknown>).status).toBe('completed');

    send({
      type: 'conversation.item.create',
      item: { type: 'function_call_output', call_id: call.call_id, output: '{"content":"hi"}' },
    });
    send({ type: 'response.create' });
    expect(types().slice(5)).toEqual([
      'response.created',
      'response.output_audio_transcript.delta',
      'response.output_audio_transcript.done',
      'response.done',
    ]);
    const transcript = emitted[7].transcript;
    expect(typeof transcript).toBe('string');
    expect((transcript as string).length).toBeGreaterThan(0);
    expect((emitted[8].response as Record<string, unknown>).status).toBe('completed');
  });

  it('does not report routed device audio arriving during an answer as a new turn of speech', () => {
    /** Would catch the defect that makes Voice Everywhere unusable: the paired
     * device keeps streaming PCM while the far end is speaking, and raising
     * `input_audio_buffer.speech_started` for it makes the session read its own
     * answer as a barge-in and tear down every response it ever produces. */
    // `defer` never fires, so the answer below is still in flight when the
    // later chunks arrive — which is the situation this guards.
    const { peer, send, types } = dialect({ tools: [], defer: () => undefined });
    peer.open();
    send({ type: 'input_audio_buffer.append', audio: 'AAAA' });
    expect(types()).toContain('input_audio_buffer.speech_started');
    send({ type: 'input_audio_buffer.commit' });
    send({ type: 'response.create' });
    const before = types().length;

    send({ type: 'input_audio_buffer.append', audio: 'BBBB' });
    send({ type: 'input_audio_buffer.append', audio: 'CCCC' });
    expect(types().slice(before)).toEqual([]);
  });

  it('reports a cancelled answer as response.done with status cancelled and silences the outbound track', () => {
    /** Would catch a far end that stays silent on `response.cancel`: the
     * response ledger would never close the abandoned response, and the tone
     * would keep sounding through a barge-in that stopped nothing. */
    const { peer, emitted, speaking, send, types } = dialect({ defer: () => undefined });
    peer.open();
    send({ type: 'input_audio_buffer.commit' });
    send({ type: 'response.create' });
    send({ type: 'conversation.item.create', item: { type: 'function_call_output' } });
    send({ type: 'response.create' });
    expect(speaking).toEqual([false, true]);

    send({ type: 'response.cancel' });
    const sent = types();
    expect(sent[sent.length - 1]).toBe('response.done');
    expect((emitted[emitted.length - 1].response as Record<string, unknown>).status).toBe('cancelled');
    expect(speaking[speaking.length - 1]).toBe(false);
  });

  it('refuses a second response while one is still in flight, the way the provider refuses it', () => {
    /** The session holds a continuation back while a response is active
     * precisely because the provider errors on a concurrent one. A far end that
     * happily opened a second response would let that hold-back rot untested. */
    const { peer, emitted, send } = dialect({ tools: [], defer: () => undefined });
    peer.open();
    send({ type: 'response.create' });
    send({ type: 'response.create' });
    const error = emitted[emitted.length - 1];
    expect(error.type).toBe('error');
    expect((error.error as Record<string, unknown>).code).toBe('conversation_already_has_active_response');
  });

  it('omits tool arguments when the tool schema requires a field the instructions never name', () => {
    /** Would catch a far end that invents an argument value: the host tool
     * executor would then read a path nobody asked for. */
    const { peer, emitted, send } = dialect({ instructions: 'Say hello.' });
    peer.open();
    send({ type: 'response.create' });
    expect((emitted[emitted.length - 2].item as Record<string, unknown>).arguments).toBe('{}');
  });

  it('commits a turn on its own once enough routed audio has arrived under provider turn detection', () => {
    /** With `semantic_vad` the host never commits, so a far end that waited for
     * one would answer a routed device exactly never. */
    const { peer, send, types } = dialect({ turnDetection: 'semantic_vad', defer: () => undefined });
    peer.open();
    for (let chunk = 0; chunk < 20; chunk += 1) send({ type: 'input_audio_buffer.append', audio: 'x'.repeat(4_000) });
    expect(types()).toContain('conversation.item.input_audio_transcription.completed');
    expect(types()).toContain('response.created');
  });
});

/* ------------------------------------------------------------------------ */
/* In-memory peers. Not WebRTC — see the note at the top of this file.        */
/* ------------------------------------------------------------------------ */

class WireChannel {
  readyState = 'connecting';
  twin: WireChannel | null = null;
  closed = false;
  private readonly listeners = new Map<string, Set<EventListener>>();
  send(data: string) { this.twin?.fire('message', { data }); }
  close() { this.closed = true; this.readyState = 'closed'; }
  addEventListener(type: string, listener: EventListener) {
    const set = this.listeners.get(type) ?? new Set<EventListener>();
    set.add(listener);
    this.listeners.set(type, set);
  }
  removeEventListener(type: string, listener: EventListener) { this.listeners.get(type)?.delete(listener); }
  fire(type: string, extra: Record<string, unknown> = {}) {
    for (const listener of [...(this.listeners.get(type) ?? [])]) listener({ type, ...extra } as unknown as Event);
  }
  open() { this.readyState = 'open'; this.fire('open'); }
}

const statsReport = (energy: number): RTCStatsReport => new Map([
  ['inbound-audio', { id: 'inbound-audio', type: 'inbound-rtp', kind: 'audio', bytesReceived: 4_000, totalSamplesReceived: 8_000, totalAudioEnergy: energy }],
]) as unknown as RTCStatsReport;

class WirePeer {
  connectionState: RTCPeerConnectionState = 'new';
  signalingState: RTCSignalingState = 'stable';
  iceGatheringState: RTCIceGatheringState = 'new';
  localDescription: RTCSessionDescriptionInit | null = null;
  remoteDescription: RTCSessionDescriptionInit | null = null;
  ontrack: ((event: RTCTrackEvent) => void) | null = null;
  closed = false;
  twin: WirePeer | null = null;
  channel: WireChannel | null = null;
  sentTrack: unknown = null;
  energy = 0;
  private readonly listeners = new Map<string, Set<EventListener>>();
  private readonly transceiver = {
    receiver: { track: { kind: 'audio' } },
    sender: { replaceTrack: vi.fn(async (track: unknown) => { this.sentTrack = track; }) },
  };

  addTransceiver() { return this.transceiver as unknown as RTCRtpTransceiver; }
  getTransceivers() { return [this.transceiver] as unknown as RTCRtpTransceiver[]; }
  addTrack() { return this.transceiver.sender as unknown as RTCRtpSender; }
  createDataChannel(label: string) {
    this.channel = new WireChannel();
    void label;
    return this.channel as unknown as RTCDataChannel;
  }
  async createOffer() { return { type: 'offer' as RTCSdpType, sdp: 'v=0\r\noffer-from-session' }; }
  async createAnswer() { return { type: 'answer' as RTCSdpType, sdp: 'v=0\r\nanswer-from-far-end' }; }
  async setLocalDescription(description: RTCSessionDescriptionInit) {
    this.localDescription = description;
    this.signalingState = description.type === 'offer' ? 'have-local-offer' : 'stable';
    this.iceGatheringState = 'complete';
    this.fire('signalingstatechange');
  }
  async setRemoteDescription(description: RTCSessionDescriptionInit) {
    this.remoteDescription = description;
    if (description.type === 'offer') {
      // The answering side learns about the offerer's data channel here.
      const twinChannel = this.twin?.channel;
      const mine = new WireChannel();
      if (twinChannel) { mine.twin = twinChannel; twinChannel.twin = mine; }
      this.channel = mine;
      this.fire('datachannel', { channel: mine });
      return;
    }
    this.signalingState = 'stable';
    this.connectionState = 'connected';
    this.fire('signalingstatechange');
    this.ontrack?.({
      streams: [{ id: 'far-end-audio' } as unknown as MediaStream],
      track: { kind: 'audio' } as unknown as MediaStreamTrack,
      receiver: { getStats: async () => statsReport(this.energy) },
    } as unknown as RTCTrackEvent);
    this.channel?.open();
    this.twin?.channel?.open();
  }
  async addIceCandidate() { }
  close() { this.closed = true; this.connectionState = 'closed'; }
  addEventListener(type: string, listener: EventListener) {
    const set = this.listeners.get(type) ?? new Set<EventListener>();
    set.add(listener);
    this.listeners.set(type, set);
  }
  removeEventListener(type: string, listener: EventListener) { this.listeners.get(type)?.delete(listener); }
  fire(type: string, extra: Record<string, unknown> = {}) {
    for (const listener of [...(this.listeners.get(type) ?? [])]) listener({ type, ...extra } as unknown as Event);
  }
}

function audioContext() {
  const oscillator = { frequency: { value: 0 }, started: false, stopped: false, connect: () => undefined, disconnect: () => undefined, start() { oscillator.started = true; }, stop() { oscillator.stopped = true; } };
  const gain = { gain: { value: -1 }, connect: () => undefined, disconnect: () => undefined };
  const track = { kind: 'audio', stop: () => undefined };
  const context = {
    oscillator, gain, closedContext: false,
    state: 'running',
    createOscillator: () => oscillator,
    createGain: () => gain,
    createMediaStreamDestination: () => ({ stream: { getAudioTracks: () => [track], getTracks: () => [track] } }),
    close: async () => { context.closedContext = true; context.state = 'closed'; },
  };
  return context;
}

function wiredProvider(overrides: { defer?: (run: () => void, ms: number) => void } = {}) {
  const peers: WirePeer[] = [];
  const contexts: ReturnType<typeof audioContext>[] = [];
  const micTrack = { enabled: true, stop: vi.fn(), addEventListener: vi.fn(), removeEventListener: vi.fn() };
  const element: Record<string, unknown> = {
    autoplay: false, srcObject: null, paused: false, currentTime: 0.5,
    addEventListener: vi.fn(), removeEventListener: vi.fn(),
    play: vi.fn(async () => undefined), pause: vi.fn(() => undefined),
  };
  const provider = new LoopbackRealtimeVoiceProvider({
    createPeerConnection: () => {
      const peer = new WirePeer();
      const [first] = peers;
      if (first) { peer.twin = first; first.twin = peer; }
      peers.push(peer);
      return peer as unknown as RTCPeerConnection;
    },
    createAudioContext: () => {
      const context = audioContext();
      contexts.push(context);
      return context as unknown as AudioContext;
    },
    getUserMedia: async () => ({
      getAudioTracks: () => [micTrack], getTracks: () => [micTrack],
    }) as unknown as MediaStream,
    createAudio: () => element as unknown as HTMLAudioElement,
    audioProbeIntervalMs: 5,
    defer: overrides.defer ?? ((run) => { run(); }),
  });
  return { provider, peers, contexts, element, micTrack };
}

function session(rig: ReturnType<typeof wiredProvider>, events: RealtimeVoiceEvent[]) {
  return rig.provider.createSession({
    sessionId: 'rv_loopback', model: 'loopback', voice: 'marin', turnDetection: 'manual',
    inputDeviceId: null, outputDeviceId: null, instructions: INSTRUCTIONS,
    tools: [{ type: 'function', function: { name: 'read_file', description: 'read a file', parameters: TOOLS[0].parameters } }],
  }, (event) => events.push(event));
}

describe('LoopbackRealtimeVoiceProvider', () => {
  afterEach(() => { vi.restoreAllMocks(); });

  it('is identified as loopback and never claims to be the OpenAI provider', () => {
    /** The contract every caller keys off — config, settings and the docs all
     * name `loopback`, and a renamed id would silently select nothing. */
    const provider = new LoopbackRealtimeVoiceProvider();
    expect(provider.id).toBe('loopback');
    expect(provider.capabilities.interruption).toBe(true);
    expect(provider.capabilities.tools).toBe(true);
  });

  it('drives the production session through a whole tool-and-speech turn with no broker and no key', () => {
    /** The claim this whole file exists for. Would catch a far end that never
     * answers the offer, never attaches to the session's data channel, or whose
     * dialect the production session cannot consume — any of which leaves Voice
     * Everywhere unprovable without an OpenAI credential. */
    const rig = wiredProvider();
    const events: RealtimeVoiceEvent[] = [];
    const controller = new RealtimeVoiceController();
    const live = session(rig, events);
    return (async () => {
      controller.connecting();
      await live.connect();
      expect(live.state).toBe('ready');
      // The answer the session applied came from the local answering peer.
      expect(rig.peers[0].remoteDescription?.sdp).toBe('v=0\r\nanswer-from-far-end');
      expect(rig.peers[1].remoteDescription?.sdp).toBe('v=0\r\noffer-from-session');

      await live.startManualTurn();
      await live.finishManualTurn();

      const consumed = events.filter((event) => controller.consume(event));
      const call = consumed.find((event) => event.type === 'tool_call');
      expect(call?.type === 'tool_call' && call.call.name).toBe('read_file');
      expect(call?.type === 'tool_call' && call.call.arguments).toBe('{"path":"docs/acceptance.md"}');
      const transcript = consumed.find((event) => event.type === 'input_transcript');
      expect(transcript?.type === 'input_transcript' && transcript.text.length).toBeGreaterThan(0);

      const before = events.length;
      live.sendToolResult(call?.type === 'tool_call' ? call.call.id : '', '{"content":"ok"}');
      expect(controller.settleToolCall(call?.type === 'tool_call' ? call.call.id : '')).toBe('continue');
      live.requestResponse();

      const spoken = events.slice(before);
      for (const event of spoken) controller.consume(event);
      expect(spoken.map((event) => event.type)).toEqual([
        'response_started',
        'output_generation_started',
        'output_transcript_delta',
        'output_transcript_done',
        'response_done',
      ]);
      const done = spoken[spoken.length - 1];
      expect(done.type === 'response_done' && done.status).toBe('completed');
      await live.close();
    })();
  });

  it('carries routed paired-device PCM to the far end over the production data channel', () => {
    /** Voice Everywhere's whole input path when the microphone is a phone:
     * `appendInputPcm16` on the production session has to reach the far end and
     * be recognised as speech, or a routed turn is never transcribed at all. */
    const rig = wiredProvider();
    const events: RealtimeVoiceEvent[] = [];
    const live = rig.provider.createSession({
      sessionId: 'rv_loopback_routed', model: 'loopback', voice: 'marin', turnDetection: 'manual',
      inputDeviceId: null, outputDeviceId: null, externalInput: true, instructions: INSTRUCTIONS, tools: [],
    }, (event) => events.push(event));
    return (async () => {
      await live.connect();
      live.appendInputPcm16('QUJDRA==');
      await live.finishManualTurn();
      const transcript = events.find((event) => event.type === 'input_transcript');
      expect(transcript?.type === 'input_transcript' && transcript.text.length).toBeGreaterThan(0);
      // With the microphone routed away, no local capture was ever opened.
      expect(rig.micTrack.stop).not.toHaveBeenCalled();
      await live.close();
    })();
  });

  it('gates the outbound tone on the answer and releases the far end when the session closes', () => {
    /** Would catch a far end that hums through the whole call — which makes a
     * barge-in unfalsifiable — and one that leaks its peer, oscillator and
     * audio context after `close()`, so a long acceptance run ends with a pile
     * of live connections. */
    const rig = wiredProvider({ defer: () => undefined });
    const events: RealtimeVoiceEvent[] = [];
    const live = session(rig, events);
    return (async () => {
      await live.connect();
      const context = rig.contexts[0];
      expect(context.oscillator.started).toBe(true);
      expect(context.gain.gain.value).toBe(0);

      await live.finishManualTurn();
      live.requestResponse();
      expect(context.gain.gain.value).toBe(1);

      await live.interrupt();
      expect(context.gain.gain.value).toBe(0);

      await live.close();
      expect(rig.peers[1].closed).toBe(true);
      expect(context.oscillator.stopped).toBe(true);
      expect(context.closedContext).toBe(true);
    })();
  });
});
