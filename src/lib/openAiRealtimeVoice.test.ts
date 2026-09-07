import { describe, expect, it, vi } from 'vitest';

import { OpenAiRealtimeVoiceProvider, normalizeOpenAiRealtimeEvent, type OpenAiRealtimeEnvironment } from './openAiRealtimeVoice';
import type { RealtimeVoiceEvent, RealtimeVoiceSessionConfig } from './realtimeVoice';

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
  addTrack() { return {} as RTCRtpSender; }
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

function harness(
  config: Partial<RealtimeVoiceSessionConfig> = {},
  audioExtras: Record<string, unknown> = {},
  brokerAnswer: () => Promise<{ sessionId: string; sdpAnswer: string; providerRequestId: string | null }> =
  async () => ({ sessionId: 'rv_test', sdpAnswer: 'v=0\r\nanswer', providerRequestId: null }),
) {
  const peer = new FakePeer();
  const track = new FakeTrack();
  const stream = { getAudioTracks: () => [track], getTracks: () => [track] } as unknown as MediaStream;
  const play = vi.fn(async () => undefined);
  const pause = vi.fn();
  const audio = {
    autoplay: false, srcObject: null, play, pause,
    addEventListener: vi.fn(), removeEventListener: vi.fn(),
    ...audioExtras,
  } as unknown as HTMLAudioElement;
  const connectBroker = vi.fn(async (_request: BrokerRequest) => brokerAnswer());
  const disconnectBroker = vi.fn(async () => undefined);
  const environment: OpenAiRealtimeEnvironment = {
    createPeer: () => peer as never,
    getUserMedia: vi.fn(async () => stream),
    createAudio: () => audio,
    connectBroker,
    disconnectBroker,
  };
  const events: RealtimeVoiceEvent[] = [];
  const session = new OpenAiRealtimeVoiceProvider(environment).createSession({
    sessionId: 'rv_test', model: 'gpt-realtime-2.1', voice: 'marin', turnDetection: 'manual',
    inputDeviceId: null, outputDeviceId: null, instructions: 'safe',
    tools: [{ type: 'function', function: { name: 'read_file', description: 'read', parameters: { type: 'object' } } }],
    ...config,
  }, (event) => events.push(event));
  return { peer, track, audio, play, pause, connectBroker, disconnectBroker, events, session };
}

const sentTypes = (peer: FakePeer): string[] => peer.channel.sent.map((value) => JSON.parse(value).type as string);

const remoteTrackEvent = () => ({ streams: [{} as MediaStream] } as unknown as RTCTrackEvent);

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

  it('anchors first output on the transcript delta the WebRTC transport does deliver', async () => {
    // Model audio arrives on the media track, so `response.output_audio.delta`
    // is a WebSocket-only event. Anchoring on it alone left first-audio
    // latency, the underrun counter and the barge-in guards dead here.
    const { peer, session, events } = harness();
    await session.connect();
    peer.channel.receive({ type: 'response.created', event_id: 'created', response: { id: 'r1' } });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'd1', item_id: 'a1', delta: 'He',
    });
    peer.channel.receive({
      type: 'response.output_audio_transcript.delta', event_id: 'd2', item_id: 'a1', delta: 'llo',
    });
    expect(events.filter((event) => event.type === 'output_audio_started')).toHaveLength(1);
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
    expect(events.filter((event) => event.type === 'output_audio_started')).toHaveLength(1);
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
