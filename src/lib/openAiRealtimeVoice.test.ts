import { describe, expect, it, vi } from 'vitest';

import { OpenAiRealtimeVoiceProvider, normalizeOpenAiRealtimeEvent, type OpenAiRealtimeEnvironment } from './openAiRealtimeVoice';
import type { RealtimeVoiceEvent } from './realtimeVoice';

class FakeChannel {
  readyState = 'open';
  sent: string[] = [];
  closed = false;
  listeners = new Map<string, EventListener>();
  send(data: string) { this.sent.push(data); }
  close() { this.closed = true; }
  addEventListener(type: string, listener: EventListener) { this.listeners.set(type, listener); }
  removeEventListener(type: string) { this.listeners.delete(type); }
  receive(payload: unknown) {
    this.listeners.get('message')?.({ data: JSON.stringify(payload) } as MessageEvent);
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
  addTrack() { return {} as RTCRtpSender; }
  createDataChannel() { return this.channel as unknown as RTCDataChannel; }
  async createOffer() { return { type: 'offer' as RTCSdpType, sdp: 'v=0\r\n' }; }
  async setLocalDescription() {}
  async setRemoteDescription() { this.connectionState = 'connected'; }
  close() { this.closed = true; }
  addEventListener() {}
  removeEventListener() {}
}

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
    })).toEqual({ type: 'tool_call', eventId: 'e2', call: { id: 'c1', itemId: 'i1', name: 'read_file', arguments: '{"path":"a"}' } });
    expect(normalizeOpenAiRealtimeEvent({ type: 'rate_limits.updated', event_id: 'stale' })).toBeNull();
  });

  it('brokers SDP, interrupts both generation and queued audio, and fully tears down', async () => {
    const peer = new FakePeer();
    const track = new FakeTrack();
    const stream = {
      getAudioTracks: () => [track], getTracks: () => [track],
    } as unknown as MediaStream;
    const audio = {
      autoplay: false, srcObject: null, play: vi.fn(async () => undefined), pause: vi.fn(),
      addEventListener: vi.fn(), removeEventListener: vi.fn(),
    } as unknown as HTMLAudioElement;
    const connectBroker = vi.fn(async () => ({ sessionId: 'rv_test', sdpAnswer: 'v=0\r\nanswer', providerRequestId: null }));
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
    }, (event) => events.push(event));

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
    expect(peer.channel.sent.map((value) => JSON.parse(value).type)).toEqual([
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
});
