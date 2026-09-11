import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { OpenAiRealtimeVoiceProvider, normalizeOpenAiRealtimeEvent, type OpenAiRealtimeEnvironment } from './openAiRealtimeVoice';
import type { RealtimeVoiceEvent, RealtimeVoiceSession, RealtimeVoiceSessionConfig } from './realtimeVoice';

class FakeChannel {
  readyState = 'open';
  sent: string[] = [];
  closed = false;
  listeners = new Map<string, EventListener>();
  send(data: string) { this.sent.push(data); }
  close() { this.closed = true; }
  addEventListener(type: string, listener: EventListener) { this.listeners.set(type, listener); }
  removeEventListener(type: string) { this.listeners.delete(type); }
  receive(payload: unknown) { this.receiveRaw(JSON.stringify(payload)); }
  receiveRaw(data: string) {
    this.listeners.get('message')?.({ data } as MessageEvent);
  }
}

class FakeTrack {
  enabled = true;
  stopped = false;
  listeners = new Map<string, EventListener>();
  addEventListener(type: string, listener: EventListener) { this.listeners.set(type, listener); }
  removeEventListener(type: string) { this.listeners.delete(type); }
  stop() { this.stopped = true; }
}

class FakePeer {
  connectionState: RTCPeerConnectionState = 'new';
  channel = new FakeChannel();
  closed = false;
  ontrack: ((event: RTCTrackEvent) => void) | null = null;
  // Real listener bookkeeping rather than a swallowed `addEventListener`: the
  // peer-failure paths are only reachable by firing the listener the session
  // registered, and a fake that drops it silently reports coverage it lacks.
  listeners = new Map<string, Set<EventListener>>();
  sender = { replaceTrack: vi.fn(async (_track: MediaStreamTrack | null) => undefined) } as unknown as RTCRtpSender;
  addTrack() { return this.sender; }
  addTransceiver() { return { sender: this.sender } as RTCRtpTransceiver; }
  createDataChannel() { return this.channel as unknown as RTCDataChannel; }
  async createOffer() { return { type: 'offer' as RTCSdpType, sdp: 'v=0\r\n' }; }
  async setLocalDescription() {}
  async setRemoteDescription() { this.connectionState = 'connected'; }
  close() { this.closed = true; }
  addEventListener(type: string, listener: EventListener) {
    const registered = this.listeners.get(type) ?? new Set<EventListener>();
    registered.add(listener);
    this.listeners.set(type, registered);
  }
  removeEventListener(type: string, listener: EventListener) { this.listeners.get(type)?.delete(listener); }
  fire(type: string) {
    for (const listener of [...(this.listeners.get(type) ?? [])]) listener({ type } as Event);
  }
}

type BrokerRequest = Parameters<OpenAiRealtimeEnvironment['connectBroker']>[0];

/** An `RTCStatsReport` is a `Map`, and the adapter only ever walks it with
 * `forEach`, so a Map keyed by stat id is the entire fake. */
const statsReport = (entries: readonly Record<string, unknown>[]): RTCStatsReport =>
  new Map(entries.map((entry, index) => [String(entry.id ?? `stat-${index}`), entry])) as unknown as RTCStatsReport;

const audioStat = (stats: Record<string, unknown>): Record<string, unknown> =>
  ({ id: 'inbound-audio', type: 'inbound-rtp', kind: 'audio', ...stats });

/** The receiver the transport hands over with the remote track. `reads()` sets
 * what the next `getStats()` resolves with, which is how a test drives the
 * probe: it only ever compares one reading against the one before it. */
function fakeReceiver() {
  let gate: Promise<void> | null = null;
  let release: (() => void) | null = null;
  const receiver = {
    report: statsReport([audioStat({ bytesReceived: 0, totalSamplesReceived: 0, totalAudioEnergy: 0 })]),
    rejects: false,
    getStats: vi.fn(async () => {
      if (receiver.rejects) throw new Error('getStats is unavailable on this platform');
      await gate;
      return receiver.report;
    }),
    reads(...entries: Record<string, unknown>[]) { receiver.report = statsReport(entries); },
    /** Parks `getStats()` so a probe can still be in flight when the session is
     * torn down under it — otherwise that race is unreachable from a test. */
    park() { gate = new Promise((resolve) => { release = resolve; }); },
    resume() { gate = null; release?.(); release = null; },
  };
  return receiver;
}

function harness(
  config: Partial<RealtimeVoiceSessionConfig> = {},
  audioExtras: Record<string, unknown> = {},
  brokerAnswer: () => Promise<{ sessionId: string; sdpAnswer: string; providerRequestId: string | null }> =
  async () => ({ sessionId: 'rv_test', sdpAnswer: 'v=0\r\nanswer', providerRequestId: null }),
  probeMs?: number,
) {
  const peer = new FakePeer();
  const track = new FakeTrack();
  const stream = { getAudioTracks: () => [track], getTracks: () => [track] } as unknown as MediaStream;
  // `paused` is modelled rather than left undefined: the acceptance run reads
  // it to decide whether the local playback path carried the answer, so a fake
  // that always reported paused would hide exactly that.
  const element: Record<string, unknown> = {
    autoplay: false, srcObject: null, paused: false,
    addEventListener: vi.fn(), removeEventListener: vi.fn(),
  };
  const play = vi.fn(async () => { element.paused = false; });
  const pause = vi.fn(() => { element.paused = true; });
  Object.assign(element, { play, pause }, audioExtras);
  const audio = element as unknown as HTMLAudioElement;
  const mic = vi.fn(async () => stream);
  const connectBroker = vi.fn(async (_request: BrokerRequest) => brokerAnswer());
  const disconnectBroker = vi.fn(async () => undefined);
  const environment: OpenAiRealtimeEnvironment = {
    createPeer: () => peer as never,
    getUserMedia: mic,
    createAudio: () => audio,
    createAudioContext: () => ({}) as AudioContext,
    connectBroker,
    disconnectBroker,
    ...(probeMs === undefined ? {} : { audioProbeIntervalMs: probeMs }),
  };
  const events: RealtimeVoiceEvent[] = [];
  const session = new OpenAiRealtimeVoiceProvider(environment).createSession({
    sessionId: 'rv_test', model: 'gpt-realtime-2.1', voice: 'marin', turnDetection: 'manual',
    inputDeviceId: null, outputDeviceId: null, instructions: 'safe',
    tools: [{ type: 'function', function: { name: 'read_file', description: 'read', parameters: { type: 'object' } } }],
    ...config,
  }, (event) => events.push(event));
  const receiver = fakeReceiver();
  const remoteStream = { id: 'remote-model-audio' } as unknown as MediaStream;
  return {
    peer, track, audio, play, pause, mic, connectBroker, disconnectBroker, events, session, receiver, remoteStream,
    attachRemoteTrack: () => peer.ontrack?.({ streams: [remoteStream], receiver } as unknown as RTCTrackEvent),
  };
}

const sentTypes = (peer: FakePeer): string[] => peer.channel.sent.map((value) => JSON.parse(value).type as string);

const remoteTrackEvent = () => ({ streams: [{} as MediaStream] } as unknown as RTCTrackEvent);

const playing = (events: readonly RealtimeVoiceEvent[]) => events.filter(
  (event): event is Extract<RealtimeVoiceEvent, { type: 'non_silent_remote_audio' }> =>
    event.type === 'non_silent_remote_audio',
);

/** `audioProgress` is optional on the session interface, and the acceptance run
 * fails its barge-in step outright without it, so a session that does not
 * implement it fails here too rather than quietly skipping the assertion. */
const progressOf = (session: RealtimeVoiceSession) => {
  if (!session.audioProgress) throw new Error('the session exposes no audioProgress()');
  return session.audioProgress();
};

const PROBE_MS = 20;

/** One probe period, plus the microtasks `probeAudioOnce` awaits before it can
 * emit: advancing the clock alone can return before `getStats()` has settled. */
const tick = async (periods = 1): Promise<void> => {
  for (let index = 0; index < periods; index += 1) {
    await vi.advanceTimersByTimeAsync(PROBE_MS);
    await Promise.resolve();
    await Promise.resolve();
  }
};

describe('OpenAI realtime response pacing', () => {
  it('holds a continuation until the response in flight finishes', async () => {
    // With provider VAD the model can open its own response while a host tool
    // is still running, so a tool result can land mid-response. The provider
    // refuses a second concurrent response outright, and the spoken follow-up
    // the operator is owed would simply never arrive.
    const { peer, session } = harness({ turnDetection: 'semantic_vad' });
    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created-2', response: { id: 'r2' } });
    session.sendToolResult('call-1', '{"ok":true}');
    session.requestResponse();
    expect(sentTypes(peer)).toEqual(['conversation.item.create']);
    peer.channel.receive({ type: 'response.done', event_id: 'done-2', response: { id: 'r2', status: 'completed' } });
    expect(sentTypes(peer)).toEqual(['conversation.item.create', 'response.create']);
    // Exactly one: a held ask is not a standing subscription to every
    // subsequent `response.done`.
    peer.channel.receive({ type: 'response.done', event_id: 'done-3', response: { id: 'r3', status: 'completed' } });
    expect(sentTypes(peer).filter((type) => type === 'response.create')).toHaveLength(1);
    await session.close();
  });

  it('drops a held continuation when the operator interrupts instead', async () => {
    const { peer, session } = harness({ turnDetection: 'semantic_vad' });
    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'r1' } });
    session.requestResponse();
    await session.interrupt();
    peer.channel.receive({ type: 'response.done', event_id: 'done', response: { id: 'r1', status: 'cancelled' } });
    expect(sentTypes(peer)).toEqual(['response.cancel', 'output_audio_buffer.clear']);
    await session.close();
  });

  it('does not report an interruption on the next manual turn after a finished answer', async () => {
    // `outputStarted` was only cleared on `response.created`, and a manual turn
    // sees no server speech events — so the next hold-to-talk emitted a
    // phantom `interrupted`, which marks live responses non-continuable and
    // silently drops the answer a tool result was owed.
    const { peer, session, events } = harness();
    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'r1' } });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'delta', item_id: 'a1', delta: 'done',
    });
    peer.channel.receive({ type: 'response.done', event_id: 'done', response: { id: 'r1', status: 'completed' } });
    await session.startManualTurn();
    expect(events.filter((event) => event.type === 'interrupted')).toHaveLength(0);
    expect(sentTypes(peer)).toEqual(['input_audio_buffer.clear']);
    await session.close();
  });

  it('anchors output generation on the transcript delta the WebRTC transport does deliver', async () => {
    // Model audio arrives on the media track, so `response.output_audio.delta`
    // is a WebSocket-only event and the transcript delta is the only
    // generation-timing signal this transport gets. It marks the model
    // starting to produce output, and nothing more — whether any of it reached
    // the speaker is `non_silent_remote_audio`, measured off the receiver.
    const { peer, session, events } = harness();
    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'r1' } });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'd1', item_id: 'a1', delta: 'He',
    });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'd2', item_id: 'a1', delta: 'llo',
    });
    expect(events.filter((event) => event.type === 'output_generation_started')).toHaveLength(1);
    await session.close();
  });

  it('gives two function calls in one response distinct event ids without a provider event id', () => {
    // `response.output_item.done` carries no `item_id`, so the fallback used to
    // collapse to the response id: the second call was deduplicated away,
    // never executed and never answered.
    const call = (id: string, callId: string) => normalizeOpenAiRealtimeEvent({
      type: 'response.output_item.done', response_id: 'r1',
      item: { type: 'function_call', id, call_id: callId, name: 'read_file', arguments: '{}' },
    });
    expect(call('i1', 'c1')?.eventId).not.toBe(call('i2', 'c2')?.eventId);
  });
});

describe('OpenAI realtime adapter', () => {
  it('reports server/manual VAD, audio, interruption, transcript, and tool capabilities', () => {
    const provider = new OpenAiRealtimeVoiceProvider({} as OpenAiRealtimeEnvironment);
    expect(provider.capabilities).toEqual(expect.objectContaining({
      serverVad: true,
      manualTurnDetection: true,
      inputAudio: true,
      outputAudio: true,
      interruption: true,
      inputTranscription: true,
      outputTranscription: true,
      tools: true,
    }));
  });

  it('normalizes transcripts and completed function calls', () => {
    expect(normalizeOpenAiRealtimeEvent({
      type: 'conversation.item.input_audio_transcription.completed', event_id: 'e1', item_id: 'u1', transcript: 'hello',
    })).toEqual({ type: 'input_transcript', eventId: 'e1', itemId: 'u1', text: 'hello' });
    expect(normalizeOpenAiRealtimeEvent({
      type: 'response.output_item.done', event_id: 'e2', item: { type: 'function_call', id: 'i1', call_id: 'c1', name: 'read_file', arguments: '{"path":"a"}' },
    })).toEqual({
      type: 'tool_call', eventId: 'e2', responseId: null,
      call: { id: 'c1', itemId: 'i1', name: 'read_file', arguments: '{"path":"a"}' },
    });
    expect(normalizeOpenAiRealtimeEvent({ type: 'rate_limits.updated', event_id: 'stale' })).toBeNull();
  });

  it('attributes a tool call to the response that asked for it, and to nothing when the provider omits it', () => {
    // The controller ledger keys a pending continuation off this id; without it
    // the call falls back to the last started response, which is only correct
    // while a single response is in flight.
    expect(normalizeOpenAiRealtimeEvent({
      type: 'response.output_item.done', event_id: 'e3', response_id: 'response-9',
      item: { type: 'function_call', id: 'i2', call_id: 'c2', name: 'read_file', arguments: '{}' },
    })).toEqual({
      type: 'tool_call', eventId: 'e3', responseId: 'response-9',
      call: { id: 'c2', itemId: 'i2', name: 'read_file', arguments: '{}' },
    });
    expect(normalizeOpenAiRealtimeEvent({
      type: 'response.output_item.done', event_id: 'e4', response_id: '',
      item: { type: 'function_call', id: 'i3', call_id: 'c3', name: 'read_file', arguments: '{}' },
    })).toEqual({
      type: 'tool_call', eventId: 'e4', responseId: null,
      call: { id: 'c3', itemId: 'i3', name: 'read_file', arguments: '{}' },
    });
  });

  it('maps each response.done status and reads an unrecognized one as completed', () => {
    const done = (status: unknown) => normalizeOpenAiRealtimeEvent({
      type: 'response.done', event_id: 'e5',
      response: { id: 'response-1', ...(status === undefined ? {} : { status }) },
    });
    for (const status of ['completed', 'cancelled', 'incomplete', 'failed'] as const) {
      expect(done(status)).toEqual({ type: 'response_done', eventId: 'e5', responseId: 'response-1', status });
    }
    // Reading an unknown status as `cancelled` would mark the response
    // non-continuable and a delivered tool result would never be spoken, so
    // the fallback has to be the permissive one.
    expect(done('in_progress')).toEqual({ type: 'response_done', eventId: 'e5', responseId: 'response-1', status: 'completed' });
    expect(done(undefined)).toEqual({ type: 'response_done', eventId: 'e5', responseId: 'response-1', status: 'completed' });
  });

  it('brokers SDP, interrupts both generation and queued audio, and fully tears down', async () => {
    const { peer, track, connectBroker, disconnectBroker, events, session } = harness();

    await session.connect();
    expect(connectBroker).toHaveBeenCalledWith(expect.objectContaining({
      providerId: 'openai', sdp: 'v=0\r\n', tools: [expect.objectContaining({ name: 'read_file' })],
    }));
    expect(JSON.stringify(connectBroker.mock.calls)).not.toContain('Bearer');
    expect(track.enabled).toBe(false);
    await session.startManualTurn();
    expect(track.enabled).toBe(true);
    await session.finishManualTurn();
    expect(track.enabled).toBe(false);
    await session.interrupt();
    session.sendToolResult('call-1', '{"ok":true}');
    session.requestResponse();
    expect(sentTypes(peer)).toEqual([
      'input_audio_buffer.clear', 'input_audio_buffer.commit', 'response.create',
      'response.cancel', 'output_audio_buffer.clear',
      'conversation.item.create', 'response.create',
    ]);
    expect(JSON.parse(peer.channel.sent[5]).item).toEqual({
      type: 'function_call_output', call_id: 'call-1', output: '{"ok":true}',
    });

    peer.channel.receive({ type: 'response.created', event_id: 'response-created', response: { id: 'response-1' } });
    peer.channel.receive({ type: 'response.output_audio.delta', event_id: 'audio-delta', item_id: 'answer-1', delta: 'ignored-over-webrtc' });
    peer.channel.receive({ type: 'response.output_audio.delta', event_id: 'audio-delta-2', item_id: 'answer-1', delta: 'ignored-over-webrtc' });
    expect(events.filter((event) => event.type === 'output_generation_started')).toHaveLength(1);
    await session.startManualTurn();
    expect(peer.channel.sent.slice(-3).map((value) => JSON.parse(value).type)).toEqual([
      'response.cancel', 'output_audio_buffer.clear', 'input_audio_buffer.clear',
    ]);
    expect(events.some((event) => event.type === 'interrupted')).toBe(true);

    peer.channel.receive({ type: 'error', event_id: 'provider-error', error: { code: 'bad', message: 'nope' } });
    expect(events[events.length - 1]).toEqual({ type: 'error', eventId: 'provider-error', code: 'bad', message: 'nope' });
    track.listeners.get('ended')?.({} as Event);
    await Promise.resolve();
    expect(events.some((event) => event.type === 'connection_lost' && event.code === 'microphone_revoked')).toBe(true);
    await session.close();
    expect(track.stopped).toBe(true);
    expect(peer.channel.closed).toBe(true);
    expect(peer.closed).toBe(true);
    expect(disconnectBroker).toHaveBeenCalledWith('rv_test');
  });

  it('keeps the microphone live and the manual turn controls inert under server VAD', async () => {
    const { peer, track, connectBroker, session } = harness({ turnDetection: 'semantic_vad' });

    await session.connect();
    expect(connectBroker).toHaveBeenCalledWith(expect.objectContaining({ turnDetection: 'semantic_vad' }));
    // The provider decides turn boundaries, so gating the track would drop the
    // operator's first words and the manual controls must not send anything
    // that would fight the server's own commit.
    expect(track.enabled).toBe(true);
    await session.startManualTurn();
    await session.finishManualTurn();
    expect(track.enabled).toBe(true);
    expect(peer.channel.sent).toEqual([]);
  });

  it('holds the microphone closed under manual VAD and commits before asking for a response', async () => {
    const { peer, track, session } = harness();

    await session.connect();
    expect(track.enabled).toBe(false);
    await session.startManualTurn();
    await session.finishManualTurn();
    // Committing after `response.create` would ask the provider to answer an
    // empty buffer, so the order here is the contract, not an accident.
    expect(sentTypes(peer)).toEqual(['input_audio_buffer.clear', 'input_audio_buffer.commit', 'response.create']);
    expect(track.enabled).toBe(false);
  });

  it('cancels generation, drops queued audio, and pauses playback when interrupted mid-answer', async () => {
    const { peer, pause, events, session } = harness();

    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'response-1' } });
    peer.channel.receive({ type: 'response.output_audio.delta', event_id: 'delta', item_id: 'answer-1', delta: 'x' });
    expect(pause).not.toHaveBeenCalled();

    await session.interrupt();
    // Cancelling generation alone leaves already-buffered audio playing, which
    // is what a barge-in sounds like when it does not work.
    expect(sentTypes(peer)).toEqual(['response.cancel', 'output_audio_buffer.clear']);
    expect(pause).toHaveBeenCalledTimes(1);
    expect(events.some((event) => event.type === 'interrupted')).toBe(true);
  });

  it('emits nothing for provider events the product does not model', async () => {
    const { peer, events, session } = harness();

    await session.connect();
    events.length = 0;
    peer.channel.receive({ type: 'rate_limits.updated', event_id: 'rl', rate_limits: [] });
    peer.channel.receive({ type: 'conversation.item.created', event_id: 'ci', item: { id: 'i1', type: 'message' } });
    peer.channel.receive({ type: 'response.output_item.done', event_id: 'oi', item: { id: 'm1', type: 'message' } });
    expect(events).toEqual([]);
  });

  it('reports a malformed data-channel payload without throwing out of the listener', async () => {
    const { peer, events, session } = harness();

    await session.connect();
    events.length = 0;
    // A throw here would escape into the WebRTC dispatch, where nothing
    // catches it and the session would look healthy while going deaf.
    expect(() => peer.channel.receiveRaw('{"type":"response.done"')).not.toThrow();
    expect(events).toEqual([expect.objectContaining({ type: 'error', code: 'malformed_provider_event' })]);
    expect(peer.channel.listeners.has('message')).toBe(true);
  });

  it('separates an unrecoverable peer failure from a transient disconnect', async () => {
    const dead = harness();
    await dead.session.connect();
    dead.peer.connectionState = 'failed';
    dead.peer.fire('connectionstatechange');
    expect(dead.events.filter((event) => event.type === 'connection_lost')).toEqual([
      expect.objectContaining({ type: 'connection_lost', recoverable: false, code: 'webrtc_failed' }),
    ]);

    const flaky = harness();
    await flaky.session.connect();
    flaky.peer.connectionState = 'disconnected';
    flaky.peer.fire('connectionstatechange');
    // Only `disconnected` is worth reconnecting into; the hook uses this flag
    // to decide whether the durable run is failed or merely resumed.
    expect(flaky.events.filter((event) => event.type === 'connection_lost')).toEqual([
      expect.objectContaining({ type: 'connection_lost', recoverable: true, code: 'webrtc_disconnected' }),
    ]);
  });

  it('closes the session when the microphone is revoked mid-conversation', async () => {
    const { peer, track, disconnectBroker, events, session } = harness();

    await session.connect();
    track.listeners.get('ended')?.({} as Event);
    expect(events.filter((event) => event.type === 'connection_lost')).toEqual([
      expect.objectContaining({ type: 'connection_lost', recoverable: false, code: 'microphone_revoked' }),
    ]);
    // Revocation is not recoverable, so holding the peer and the broker session
    // open would only bill for a conversation nobody can be heard in.
    expect(peer.closed).toBe(true);
    expect(peer.channel.closed).toBe(true);
    expect(disconnectBroker).toHaveBeenCalledWith('rv_test');
    expect(session.state).toBe('closed');
  });

  it('survives a route move that swaps the local microphone out, and still dies when the routed one is revoked', async () => {
    /** The local track is ended on purpose when the conversation moves to a
     * paired device. A teardown keyed on *any* track ending — which is what a
     * single shared `ended` listener amounts to — would drop the very call the
     * move was meant to carry over, and the operator would hear it die as they
     * walked to the other device. The mirror defect is just as bad: dropping
     * the listener on a swap and never re-arming it leaves the new microphone's
     * revocation unnoticed, which is the leak this file's other test names. */
    const { peer, track, disconnectBroker, events, session, mic } = harness();
    await session.connect();

    await session.setInputRoute(true, null);
    expect(track.stopped).toBe(true);
    track.listeners.get('ended')?.({} as Event);
    expect(events.filter((event) => event.type === 'connection_lost')).toEqual([]);
    expect(peer.closed).toBe(false);
    expect(session.state).toBe('ready');

    const routed = new FakeTrack();
    mic.mockResolvedValue({ getAudioTracks: () => [routed], getTracks: () => [routed] } as unknown as MediaStream);
    await session.setInputRoute(false, 'mic-2');
    routed.listeners.get('ended')?.({} as Event);
    expect(events.filter((event) => event.type === 'connection_lost')).toEqual([
      expect.objectContaining({ type: 'connection_lost', recoverable: false, code: 'microphone_revoked' }),
    ]);
    expect(peer.closed).toBe(true);
    expect(peer.channel.closed).toBe(true);
    expect(disconnectBroker).toHaveBeenCalledWith('rv_test');
    expect(session.state).toBe('closed');
  });

  it('leaks no peer, track, or broker session when the broker refuses the offer', async () => {
    const { peer, track, disconnectBroker, session } = harness(
      {}, {}, async () => { throw new Error('realtime broker is not configured'); },
    );

    await expect(session.connect()).rejects.toThrow('realtime broker is not configured');
    expect(peer.closed).toBe(true);
    expect(peer.channel.closed).toBe(true);
    expect(track.stopped).toBe(true);
    expect(disconnectBroker).toHaveBeenCalledWith('rv_test');
    expect(session.state).toBe('closed');
  });

  it('sends only session and SDP shaped fields to the broker', async () => {
    const { connectBroker, session } = harness();

    await session.connect();
    // The ephemeral credential lives entirely in the Rust broker; anything
    // key-shaped reaching this payload means it also reached the webview.
    expect(Object.keys(connectBroker.mock.calls[0][0]).sort()).toEqual([
      'instructions', 'model', 'providerId', 'sdp', 'sessionId', 'tools', 'turnDetection', 'voice',
    ]);
    const payload = JSON.stringify(connectBroker.mock.calls);
    expect(payload).not.toContain('Bearer');
    expect(payload).not.toContain('sk-');
    expect(payload).not.toMatch(/api[_-]?key/i);
  });

  it('routes model audio to the chosen output device, and tolerates a browser without sink selection', async () => {
    const setSinkId = vi.fn(async () => undefined);
    const routed = harness({ outputDeviceId: 'headset-2' }, { setSinkId });
    await routed.session.connect();
    routed.peer.ontrack?.(remoteTrackEvent());
    expect(setSinkId).toHaveBeenCalledWith('headset-2');

    // Sink selection is unavailable in some webviews; the answer still has to
    // be audible on the system default rather than failing the turn.
    const plain = harness({ outputDeviceId: 'headset-2' });
    await plain.session.connect();
    expect(() => plain.peer.ontrack?.(remoteTrackEvent())).not.toThrow();
    expect(plain.play).toHaveBeenCalled();
  });
});

/** A response the model has begun producing output for. The two events always
 * travel together on this transport, and the probe refuses to confirm playback
 * unless generation has started, so every measurement test needs both. */
const beginResponse = (peer: FakePeer, responseId: string): void => {
  peer.channel.receive({ type: 'response.created', event_id: `created-${responseId}`, response: { id: responseId } });
  peer.channel.receive({
    type: 'response.output_audio_transcript.delta',
    event_id: `delta-${responseId}`, item_id: `item-${responseId}`, delta: 'The file says',
  });
};

describe('OpenAI realtime measured audio output', () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); });

  it('does not claim audio reached the speaker from a transcript delta alone', async () => {
    // The false pass the reviewer found. Over WebRTC the spoken answer travels
    // on the remote media track, so a transcript delta proves only that the
    // model generated text: with no track and no advancing receiver statistics
    // an acceptance run could report real audio output on a session whose
    // speaker never made a sound.
    const { peer, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    beginResponse(peer, 'r1');
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'd2', item_id: 'item-r1', delta: ' hello',
    });
    await tick(8);
    expect(events.filter((event) => event.type === 'output_generation_started')).toHaveLength(1);
    expect(playing(events)).toEqual([]);
    expect(events.filter((event) => event.type === 'remote_audio_track')).toEqual([]);
    await session.close();
  });

  it('reports the remote track, routes it into the element, and starts measuring from there', async () => {
    const { audio, receiver, remoteStream, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    expect(receiver.getStats).not.toHaveBeenCalled();

    attachRemoteTrack();
    expect(events.filter((event) => event.type === 'remote_audio_track')).toHaveLength(1);
    expect(audio.srcObject).toBe(remoteStream);
    // Nothing can measure playback before the track exists, so the probe
    // starting here is what makes every later measurement reachable at all.
    await tick();
    expect(receiver.getStats).toHaveBeenCalledTimes(1);

    // A renegotiation fires `ontrack` again; a second interval would double
    // every sampling period and race itself over `lastProgress`.
    attachRemoteTrack();
    await tick();
    expect(receiver.getStats).toHaveBeenCalledTimes(2);
    await session.close();
  });

  it('emits no playing event while receiver statistics stand still', async () => {
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    receiver.reads(audioStat({ bytesReceived: 4_000, totalSamplesReceived: 8_000, totalAudioEnergy: 0.4 }));
    beginResponse(peer, 'r1');

    // Generation is under way and a track exists, so this is exactly the case
    // a credulous probe would confirm: a receiver whose readings never move is
    // a stalled one, and the operator is hearing nothing.
    await tick(6);
    expect(playing(events)).toEqual([]);

    receiver.reads(audioStat({ bytesReceived: 9_000, totalSamplesReceived: 20_000, totalAudioEnergy: 0.9 }));
    await tick();
    expect(playing(events)).toHaveLength(1);
    await session.close();
  });

  it('emits no playing event for statistics that advance before the model generates anything', async () => {
    // Comfort noise and keepalive packets arrive as soon as the track is up.
    // Confirming on those would time first audio to the connection rather than
    // to the answer, and would report playback for a turn that never spoke.
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    receiver.reads(audioStat({ bytesReceived: 800, totalSamplesReceived: 1_000, totalAudioEnergy: 0.01 }));
    await tick();
    receiver.reads(audioStat({ bytesReceived: 1_600, totalSamplesReceived: 3_000, totalAudioEnergy: 0.05 }));
    await tick(3);
    expect(playing(events)).toEqual([]);

    beginResponse(peer, 'r1');
    receiver.reads(audioStat({ bytesReceived: 6_400, totalSamplesReceived: 24_000, totalAudioEnergy: 0.42 }));
    await tick();
    expect(playing(events)).toHaveLength(1);
    await session.close();
  });

  it('never accepts a rising sample count as evidence of sound', async () => {
    // `totalSamplesReceived` counts samples whether or not they hold any
    // signal, so allowing it as a fallback would let a silent stream certify
    // itself as audible on exactly the webviews that report no energy.
    const { session, receiver, events, attachRemoteTrack, peer } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'r1' } });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'delta', item_id: 'a1', delta: 'hi',
    });
    receiver.reads({
      id: 'inbound-audio', type: 'inbound-rtp', kind: 'audio',
      bytesReceived: 1_000, totalSamplesReceived: 10_000,
    });
    await tick();
    receiver.reads({
      id: 'inbound-audio', type: 'inbound-rtp', kind: 'audio',
      bytesReceived: 9_000, totalSamplesReceived: 20_000,
    });
    await tick(3);
    expect(playing(events)).toHaveLength(0);
    // A positive control, so the assertion above cannot pass because the probe
    // was never running: the same stream with energy does confirm.
    receiver.reads(audioStat({ bytesReceived: 18_000, totalSamplesReceived: 30_000, totalAudioEnergy: 0.3 }));
    await tick(2);
    expect(playing(events)).toHaveLength(1);
    await session.close();
  });

  it('does not count a track carrying silence as audio the operator can hear', async () => {
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, { currentTime: 1.5 }, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    beginResponse(peer, 'r1');
    receiver.reads(audioStat({ bytesReceived: 1_000, totalSamplesReceived: 5_000, totalAudioEnergy: 0.5 }));
    await tick();

    // Packets keep flowing on a muted or silence-filled track, so bytes alone
    // measure the network, not the speaker. Only rendered samples and
    // accumulated energy say the answer was audible.
    receiver.reads(audioStat({ bytesReceived: 9_000, totalSamplesReceived: 5_000, totalAudioEnergy: 0.5 }));
    await tick(3);
    expect(playing(events)).toEqual([]);

    receiver.reads(audioStat({ bytesReceived: 12_000, totalSamplesReceived: 5_000, totalAudioEnergy: 0.62 }));
    await tick();
    expect(playing(events).map((event) => event.progress)).toEqual([{
      bytesReceived: 12_000, samplesReceived: 5_000, audioEnergy: 0.62, audioEnergyReported: true,
      playbackPaused: false, playbackStarted: true, playbackSeconds: 1.5,
    }]);
    await session.close();
  });

  it('emits the playing event once per response and re-arms it on the next one', async () => {
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    beginResponse(peer, 'r1');
    receiver.reads(audioStat({ bytesReceived: 1_000, totalSamplesReceived: 8_000, totalAudioEnergy: 0.1 }));
    await tick();
    receiver.reads(audioStat({ bytesReceived: 2_000, totalSamplesReceived: 16_000, totalAudioEnergy: 0.2 }));
    await tick();
    expect(playing(events)).toHaveLength(1);

    // The rest of the answer keeps the statistics climbing for as long as it
    // plays; one event per response is a latency anchor, not a sample stream.
    receiver.reads(audioStat({ bytesReceived: 3_000, totalSamplesReceived: 24_000, totalAudioEnergy: 0.3 }));
    await tick(2);
    receiver.reads(audioStat({ bytesReceived: 4_000, totalSamplesReceived: 32_000, totalAudioEnergy: 0.4 }));
    await tick(2);
    expect(playing(events)).toHaveLength(1);

    // The next turn has to be able to prove its own audio: a session that
    // confirmed once and never again cannot show a later turn going silent.
    peer.channel.receive({ type: 'response.done', event_id: 'done-r1', response: { id: 'r1', status: 'completed' } });
    beginResponse(peer, 'r2');
    receiver.reads(audioStat({ bytesReceived: 5_000, totalSamplesReceived: 40_000, totalAudioEnergy: 0.5 }));
    await tick();
    expect(playing(events)).toHaveLength(2);
    await session.close();
  });

  it('separates a webview that reports no audio energy from a stream that was silent', async () => {
    // WebKit's getStats is narrower than Chromium's and this ships to WKWebView
    // and WebKitGTK too. Zeros there mean "not reported", so a caller must be
    // able to tell that apart from measured silence — otherwise a working
    // answer reads as a broken one on those platforms.
    const { session, receiver, attachRemoteTrack } = harness();
    await session.connect();
    attachRemoteTrack();
    receiver.reads({ id: 'inbound-audio', type: 'inbound-rtp', kind: 'audio', bytesReceived: 9_000 });
    const bare = await progressOf(session);
    expect(bare).toMatchObject({
      bytesReceived: 9_000, audioEnergy: 0, samplesReceived: 0, audioEnergyReported: false,
    });
    receiver.reads(audioStat({ bytesReceived: 9_000, totalAudioEnergy: 0 }));
    expect(await progressOf(session)).toMatchObject({ audioEnergyReported: true });
    await session.close();
  });

  it('sums inbound audio statistics, ignores every other entry, and reports the element position', async () => {
    const { receiver, attachRemoteTrack, session } = harness({}, { currentTime: 4.25 });
    await session.connect();
    attachRemoteTrack();
    receiver.reads(
      audioStat({ id: 'a1', bytesReceived: 1_200, totalSamplesReceived: 24_000, totalAudioEnergy: 0.5 }),
      audioStat({ id: 'a2', bytesReceived: 300, totalSamplesReceived: 6_000, totalAudioEnergy: 0.25 }),
      // A video receiver's bytes are not audio; an outbound entry is the
      // operator's own microphone, which is loud precisely when the model is
      // being interrupted. Counting either would make a mute answer measurable.
      { id: 'v1', type: 'inbound-rtp', kind: 'video', bytesReceived: 900_000, totalSamplesReceived: 90, totalAudioEnergy: 9 },
      { id: 'o1', type: 'outbound-rtp', kind: 'audio', bytesReceived: 500_000, totalSamplesReceived: 70, totalAudioEnergy: 7 },
      { id: 'x1', type: 'remote-inbound-rtp', kind: 'audio', bytesReceived: 40, totalSamplesReceived: 4, totalAudioEnergy: 4 },
    );
    await expect(progressOf(session)).resolves.toEqual({
      bytesReceived: 1_500, samplesReceived: 30_000, audioEnergy: 0.75, audioEnergyReported: true,
      playbackPaused: false, playbackStarted: true, playbackSeconds: 4.25,
    });

    // Unmeasurable is not silent. Null is what makes the barge-in check fail
    // loudly on a transport it cannot read instead of recording a quiet pass.
    receiver.rejects = true;
    await expect(progressOf(session)).resolves.toBeNull();
    await session.close();

    const detached = harness();
    await detached.session.connect();
    await expect(progressOf(detached.session)).resolves.toBeNull();
    await detached.session.close();
  });

  it('counts a provider-VAD barge-in as an interruption and an opening utterance as none', async () => {
    const { peer, pause, events, session } = harness({ turnDetection: 'semantic_vad' }, {}, undefined, PROBE_MS);
    await session.connect();

    // The operator's first words, with nothing in flight. Counting this would
    // set the interruption metric on every ordinary hands-free turn and the
    // number would stop meaning anything.
    peer.channel.receive({ type: 'input_audio_buffer.speech_started', event_id: 's1', item_id: 'u1' });
    expect(events.filter((event) => event.type === 'speech_started')).toHaveLength(1);
    expect(events.filter((event) => event.type === 'interrupted')).toEqual([]);

    beginResponse(peer, 'r1');
    // Pausing an idle element is a no-op, so the adapter does it on every
    // detected utterance; what matters is that the one interrupting an answer
    // in progress silences the queued audio still on its way to the speaker.
    const pausesBeforeBargeIn = pause.mock.calls.length;
    peer.channel.receive({ type: 'input_audio_buffer.speech_started', event_id: 's2', item_id: 'u2' });
    // Provider VAD cut the answer off mid-sentence. This handler cleared the
    // in-flight flags but emitted no normalized event, so the metric counted
    // only a local Stop and every hands-free barge-in was invisible.
    expect(events.filter((event) => event.type === 'interrupted')).toHaveLength(1);
    expect(pause.mock.calls.length).toBe(pausesBeforeBargeIn + 1);

    peer.channel.receive({ type: 'response.created', event_id: 'created-r2', response: { id: 'r2' } });
    peer.channel.receive({ type: 'response.done', event_id: 'done-r2', response: { id: 'r2', status: 'completed' } });
    peer.channel.receive({ type: 'input_audio_buffer.speech_started', event_id: 's3', item_id: 'u3' });
    expect(events.filter((event) => event.type === 'interrupted')).toHaveLength(1);
    await session.close();
  });

  it('stops probing on close, and a probe still in flight cannot emit against a torn down session', async () => {
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    beginResponse(peer, 'r1');
    receiver.reads(audioStat({ bytesReceived: 100, totalSamplesReceived: 1_000, totalAudioEnergy: 0.1 }));
    await tick();

    // Parked mid-call across the teardown, and the reading it will come back
    // with says audio advanced — so only the close-time guards stand between a
    // released peer connection and an event claiming it is playing audio.
    receiver.reads(audioStat({ bytesReceived: 900, totalSamplesReceived: 9_000, totalAudioEnergy: 0.9 }));
    receiver.park();
    await tick();
    await session.close();
    receiver.resume();
    await tick(4);
    expect(playing(events)).toEqual([]);

    // An interval outliving `close()` keeps a webview timer polling statistics
    // for a session that has already dropped its peer connection.
    const settled = receiver.getStats.mock.calls.length;
    await tick(10);
    expect(receiver.getStats.mock.calls.length).toBe(settled);
  });

  it('stays quiet after an interruption until the next response begins', async () => {
    const { peer, receiver, attachRemoteTrack, events, session } = harness({}, {}, undefined, PROBE_MS);
    await session.connect();
    attachRemoteTrack();
    beginResponse(peer, 'r1');
    receiver.reads(audioStat({ bytesReceived: 1_000, totalSamplesReceived: 8_000, totalAudioEnergy: 0.1 }));
    await tick();
    receiver.reads(audioStat({ bytesReceived: 2_000, totalSamplesReceived: 16_000, totalAudioEnergy: 0.2 }));
    await tick();
    expect(playing(events)).toHaveLength(1);

    await session.interrupt();
    // Buffered audio drains for a moment after a barge-in, so the receiver
    // keeps advancing. Confirming there would report the abandoned answer as
    // freshly playing and mask the stall the barge-in check is looking for.
    receiver.reads(audioStat({ bytesReceived: 3_000, totalSamplesReceived: 24_000, totalAudioEnergy: 0.3 }));
    await tick(2);
    receiver.reads(audioStat({ bytesReceived: 4_000, totalSamplesReceived: 32_000, totalAudioEnergy: 0.4 }));
    await tick(2);
    expect(playing(events)).toHaveLength(1);

    beginResponse(peer, 'r2');
    receiver.reads(audioStat({ bytesReceived: 5_000, totalSamplesReceived: 40_000, totalAudioEnergy: 0.5 }));
    await tick();
    expect(playing(events)).toHaveLength(2);
    await session.close();
  });
});
