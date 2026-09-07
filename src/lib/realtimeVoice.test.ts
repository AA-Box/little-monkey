import { describe, expect, it, vi } from 'vitest';

import { boundedRealtimeContext, RealtimeVoiceController } from './realtimeVoice';

describe('RealtimeVoiceController', () => {
  it('syncs only a bounded tail of durable text without tool/audio replay', () => {
    const messages = Array.from({ length: 15 }, (_, index) => ({
      role: index % 2 === 0 ? 'user' as const : 'assistant' as const,
      content: `message-${index}-${'å'.repeat(20)}`,
    }));
    messages.splice(7, 0, { role: 'tool' as never, content: 'secret tool output' });
    const context = boundedRealtimeContext(messages, 4, 180);
    expect(context).not.toContain('message-0');
    expect(context).toContain('message-14');
    expect(context).not.toContain('secret tool output');
    expect(new TextEncoder().encode(context).byteLength).toBeLessThanOrEqual(180);
    expect(boundedRealtimeContext(messages, 0, 180)).toBe('');
    expect(boundedRealtimeContext(messages, 4, 0)).toBe('');
  });

  it('tracks a complete voice turn and deduplicates replayed events', () => {
    vi.spyOn(performance, 'now').mockReturnValueOnce(10).mockReturnValueOnce(30).mockReturnValueOnce(50).mockReturnValueOnce(80);
    const controller = new RealtimeVoiceController();
    controller.connecting();
    expect(controller.consume({ type: 'connected', eventId: '1' })).toBe(true);
    expect(controller.state).toBe('ready');
    controller.consume({ type: 'speech_started', eventId: '2' });
    controller.consume({ type: 'response_started', eventId: '3', responseId: 'r1' });
    controller.consume({ type: 'output_transcript_delta', eventId: '4', itemId: 'a1', delta: 'Hi' });
    controller.consume({ type: 'output_audio_started', eventId: 'audio' });
    controller.consume({ type: 'output_underrun', eventId: 'underrun' });
    controller.consume({ type: 'response_done', eventId: '5', responseId: 'r1' });
    expect(controller.state).toBe('ready');
    expect(controller.metrics.connectionMs).not.toBeNull();
    expect(controller.metrics.firstRecognizedSpeechMs).not.toBeNull();
    expect(controller.metrics.firstModelEventMs).not.toBeNull();
    expect(controller.metrics.firstAudioMs).not.toBeNull();
    expect(controller.metrics.outputUnderruns).toBe(1);
    expect(controller.consume({ type: 'response_done', eventId: '5', responseId: 'r1' })).toBe(false);
  });

  it('represents approval, interruption, reconnect, failure, and close explicitly', () => {
    const controller = new RealtimeVoiceController();
    controller.connecting();
    controller.consume({ type: 'tool_call', eventId: 'tool', call: { id: 'c', itemId: 'i', name: 'read_file', arguments: '{}' } });
    expect(controller.state).toBe('awaiting_approval');
    controller.consume({ type: 'interrupted', eventId: 'interrupt' });
    expect(controller.metrics.interrupted).toBe(true);
    controller.consume({ type: 'connection_lost', eventId: 'lost', recoverable: true, code: 'webrtc_disconnected' });
    expect(controller.state).toBe('reconnecting');
    controller.connecting(true);
    expect(controller.metrics.reconnectCount).toBe(1);
    controller.consume({ type: 'error', eventId: 'error', code: 'credential_expired', message: 'expired' });
    expect(controller.state).toBe('error');
    // A user retry creates a fresh provider session/broker call, so a rotated
    // key can recover without ever handing a credential to this controller.
    controller.connecting(true);
    controller.consume({ type: 'connected', eventId: 'credential-refreshed' });
    expect(controller.state).toBe('ready');
    controller.close();
    expect(controller.state).toBe('closed');
    controller.reset();
    expect(controller.state).toBe('idle');
    expect(controller.metrics.reconnectCount).toBe(0);
    expect(controller.metrics.interrupted).toBe(false);
    expect(controller.consume({ type: 'connected', eventId: 'tool' })).toBe(true);
  });
});
