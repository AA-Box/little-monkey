import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  boundedRealtimeContext, RealtimeVoiceController,
  type RealtimeAudioProgress, type RealtimeVoiceEvent,
} from './realtimeVoice';

// Every timing test drives `performance.now()` through a fixed queue, so a
// leaked spy would hand the next test the tail of someone else's clock.
afterEach(() => {
  vi.restoreAllMocks();
});

/** A controller that has already reached the point where a provider response is
 * streaming, which is the only state in which tool calls and `response.done`
 * can arrive. Every ledger test starts here so the events it feeds are ones a
 * real session could actually emit in that order. */
function respondingController(responseId = 'r1'): RealtimeVoiceController {
  const controller = new RealtimeVoiceController();
  controller.connecting();
  controller.consume({ type: 'connected', eventId: 'connected' });
  controller.consume({ type: 'listening', eventId: 'listening' });
  controller.consume({ type: 'response_started', eventId: `started:${responseId}`, responseId });
  return controller;
}

/** A plausible reading of the transport's own statistics. Only the shape and
 * the fact that it advances matter to the reducer, which never inspects the
 * numbers — it trusts the adapter to emit this only once they moved. */
function playedAudio(overrides: Partial<RealtimeAudioProgress> = {}): RealtimeAudioProgress {
  return {
    bytesReceived: 4_096, samplesReceived: 24_000, audioEnergy: 0.42, playbackSeconds: 0.5,
    measured: true, ...overrides,
  };
}

function toolCall(callId: string, responseId: string | null): RealtimeVoiceEvent {
  return {
    type: 'tool_call',
    eventId: `tool:${callId}`,
    responseId,
    call: { id: callId, itemId: `item:${callId}`, name: 'read_file', arguments: '{"path":"a"}' },
  };
}

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
    // Seven readings, in the order the reducer takes them: `connecting`,
    // `connected`, `speech_started` (the connection-relative latency, then the
    // turn clock it restarts), `response_started`, `output_audio_playing` and
    // `response_done`. The generation, track and transcript events read the
    // clock not at all, which is why they are absent from this queue.
    vi.spyOn(performance, 'now')
      .mockReturnValueOnce(10).mockReturnValueOnce(30)
      .mockReturnValueOnce(50).mockReturnValueOnce(80)
      .mockReturnValueOnce(100).mockReturnValueOnce(140).mockReturnValueOnce(200);
    const controller = new RealtimeVoiceController();
    controller.connecting();
    expect(controller.consume({ type: 'connected', eventId: '1' })).toBe(true);
    expect(controller.state).toBe('ready');
    controller.consume({ type: 'speech_started', eventId: '2' });
    controller.consume({ type: 'response_started', eventId: '3', responseId: 'r1' });
    controller.consume({ type: 'output_transcript_delta', eventId: '4', itemId: 'a1', delta: 'Hi' });
    controller.consume({ type: 'output_generation_started', eventId: 'generation' });
    controller.consume({ type: 'remote_audio_track', eventId: 'track' });
    // The one event in this turn that says a human could hear anything.
    controller.consume({ type: 'output_audio_playing', eventId: 'playing', progress: playedAudio() });
    controller.consume({ type: 'output_underrun', eventId: 'underrun' });
    controller.consume({ type: 'response_done', eventId: '5', responseId: 'r1', status: 'completed' });
    expect(controller.state).toBe('ready');
    expect(controller.metrics.connectionMs).toBe(20);
    expect(controller.metrics.firstRecognizedSpeechMs).toBe(40);
    expect(controller.metrics.firstModelEventMs).toBe(20);
    // Measured from the turn clock at 80 to the playback reading at 140, not
    // from the transcript delta or the generation signal that preceded both.
    expect(controller.metrics.firstAudioMs).toBe(60);
    expect(controller.metrics.endToEndMs).toBe(120);
    expect(controller.metrics.outputUnderruns).toBe(1);
    expect(controller.consume({ type: 'response_done', eventId: '5', responseId: 'r1', status: 'completed' })).toBe(false);
  });

  it('reports a generating model with a silent speaker as responding with no first-audio latency', () => {
    // The defect this splits apart: first audio used to be anchored on a
    // transcript delta, so a session whose remote track carried nothing
    // produced exactly the same metrics as one the operator could hear. A
    // model talking to a dead speaker now has to be visible, and the only way
    // it can be is a `firstAudioMs` that stays null while the state advances.
    const controller = respondingController();
    controller.consume({ type: 'output_transcript_delta', eventId: 'delta', itemId: 'a1', delta: 'Hi' });
    controller.consume({ type: 'output_generation_started', eventId: 'generation' });
    controller.consume({ type: 'remote_audio_track', eventId: 'track' });
    expect(controller.state).toBe('responding');
    expect(controller.metrics.firstAudioMs).toBeNull();
    // And the null is a real absence of playback rather than a turn clock that
    // never started: the same turn did measure its first model event, so an
    // `output_audio_playing` arriving here would have been measured too.
    expect(controller.metrics.firstModelEventMs).not.toBeNull();
  });

  it('takes remote_audio_track as evidence only, changing no state and no metric', () => {
    // Over WebRTC this is the one channel audio can arrive on, so the
    // acceptance harness needs to see it — but a track existing is not a track
    // carrying sound, and the reducer must not upgrade one into the other.
    const controller = respondingController();
    const before = structuredClone(controller.metrics);
    const state = controller.state;
    expect(controller.consume({ type: 'remote_audio_track', eventId: 'track' })).toBe(true);
    expect(controller.state).toBe(state);
    expect(controller.metrics).toEqual(before);
    // A reconnect replaying the transport events must not read as a second
    // track arriving; deduplication is not waived for the events that no-op.
    expect(controller.consume({ type: 'remote_audio_track', eventId: 'track' })).toBe(false);
  });

  it('anchors firstAudioMs on the first measured playback and never moves it again', () => {
    vi.spyOn(performance, 'now')
      .mockReturnValueOnce(10).mockReturnValueOnce(30)
      .mockReturnValueOnce(100).mockReturnValueOnce(180).mockReturnValueOnce(900);
    const controller = new RealtimeVoiceController();
    controller.connecting();
    controller.consume({ type: 'connected', eventId: 'connected' });
    controller.consume({ type: 'listening', eventId: 'listening' });
    controller.consume({ type: 'output_audio_playing', eventId: 'playing:1', progress: playedAudio() });
    expect(controller.state).toBe('responding');
    expect(controller.metrics.firstAudioMs).toBe(80);
    // The probe keeps sampling for as long as the answer plays. Latency means
    // the moment sound started, so a later reading is not allowed to restate
    // it — nor to be mistaken for the first one because its numbers are bigger.
    controller.consume({
      type: 'output_audio_playing', eventId: 'playing:2',
      progress: playedAudio({ audioEnergy: 3.9, samplesReceived: 96_000, playbackSeconds: 2 }),
    });
    expect(controller.metrics.firstAudioMs).toBe(80);
  });

  it('counts an interruption from the provider-VAD barge-in as well as a local stop', () => {
    // The adapter synthesises `interrupted` from
    // `input_audio_buffer.speech_started` with a `:barge-in` event id. Before
    // that it emitted nothing there, so the metric only ever saw the local Stop
    // button and a talked-over answer was reported as an untouched turn. The
    // reducer must not care which of the two produced the event.
    const bargedIn = respondingController();
    bargedIn.consume({ type: 'output_generation_started', eventId: 'generation' });
    expect(bargedIn.consume({ type: 'interrupted', eventId: 'event_C9RtE0mhSjA:barge-in' })).toBe(true);
    expect(bargedIn.metrics.interrupted).toBe(true);
    expect(bargedIn.state).toBe('listening');
    const stopped = respondingController('r2');
    stopped.consume({ type: 'interrupted', eventId: 'local:interrupt:6f1d9c2a' });
    expect(stopped.metrics.interrupted).toBe(true);
  });

  it('represents approval, interruption, reconnect, failure, and close explicitly', () => {
    const controller = new RealtimeVoiceController();
    controller.connecting();
    controller.consume({ type: 'tool_call', eventId: 'tool', responseId: null, call: { id: 'c', itemId: 'i', name: 'read_file', arguments: '{}' } });
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

  describe('response ledger', () => {
    it('Blocker 1: a tool result that lands before response.done still gets its continuation', () => {
      // The tool finished while the model was still streaming, so nothing was
      // outstanding by the time the response closed. The old counter treated
      // that as "no tools this turn" and the answer was never requested.
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      expect(controller.settleToolCall('c1')).toBe('wait');
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('continue');
    });

    it('defers to the tool when the result lands after response.done', () => {
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('await_tools');
      expect(controller.settleToolCall('c1')).toBe('continue');
    });

    it('asks for the follow-up once when one response requested two function calls', () => {
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume(toolCall('c2', 'r1'));
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('await_tools');
      // The first result cannot trigger the continuation: the model would
      // answer with half its evidence and the second result would be orphaned.
      expect(controller.settleToolCall('c1')).toBe('wait');
      expect(controller.settleToolCall('c2')).toBe('continue');
    });

    it('never fires a second continuation for a response it already continued', () => {
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      expect(controller.settleToolCall('c1')).toBe('wait');
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('continue');
      // A retried send or a duplicated provider event must not double-prompt:
      // two continuations mean two spoken answers to one question.
      expect(controller.settleToolCall('c1')).toBe('closed');
      expect(controller.responseDisposition('r1')).toBe('complete');
    });

    it('never continues a cancelled response and leaves a barge-in listening', () => {
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume({ type: 'speech_started', eventId: 'barge', itemId: 'item:barge' });
      expect(controller.state).toBe('listening');
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'cancelled' });
      // Reporting `ready` here would drop the operator's in-flight utterance on
      // the floor: the microphone is open and the UI must keep showing that.
      expect(controller.state).toBe('listening');
      expect(controller.responseDisposition('r1')).toBe('complete');
      expect(controller.settleToolCall('c1')).toBe('closed');
    });

    it('never continues a failed response', () => {
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'failed' });
      expect(controller.responseDisposition('r1')).toBe('complete');
      expect(controller.settleToolCall('c1')).toBe('closed');
    });

    it('abandons a response interrupted while its tool was still running', () => {
      // The barge-in happens between the call and its result, so the ledger has
      // to remember that this response is dead — otherwise the late result
      // triggers the answer to the question the operator already talked over.
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume({ type: 'interrupted', eventId: 'interrupt' });
      expect(controller.settleToolCall('c1')).toBe('closed');
      expect(controller.responseDisposition('r1')).toBe('complete');
      expect(controller.state).toBe('listening');
    });

    it('completes an ordinary response that requested no function calls', () => {
      const controller = respondingController();
      controller.consume({ type: 'output_transcript_done', eventId: 'text', itemId: 'a1', text: 'Hello' });
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('complete');
      expect(controller.hasOutstandingToolCalls()).toBe(false);
    });

    it('attributes an unattributed tool call to the last started response', () => {
      // Some providers omit `response_id` on the function-call item. Falling
      // back to the response in flight keeps the call continuable; a synthetic
      // owner would strand it and the turn would never speak.
      const controller = respondingController();
      controller.consume(toolCall('c1', null));
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r1', status: 'completed' });
      expect(controller.responseDisposition('r1')).toBe('await_tools');
      expect(controller.settleToolCall('c1')).toBe('continue');
    });

    it('completes a response.done for an unknown response id without throwing', () => {
      // A reconnect can deliver the tail of a session this controller never saw
      // the start of; that must be an ordinary finished turn, not a crash.
      const controller = respondingController();
      expect(() => controller.consume({
        type: 'response_done', eventId: 'stale', responseId: 'r-stale', status: 'completed',
      })).not.toThrow();
      expect(controller.responseDisposition('r-stale')).toBe('complete');
      expect(controller.responseDisposition('r-never-seen')).toBe('complete');
    });

    it('says the response is closed when an answer can no longer be spoken', () => {
      // The three settlements are not interchangeable: the hook asks for the
      // follow-up on `continue`, does nothing on `wait`, and closes the durable
      // turn out on `closed`. Collapsing the last two into one boolean is what
      // made the finalize path either fire mid-turn or never fire at all.
      const controller = respondingController();
      controller.consume(toolCall('c1', 'r1'));
      controller.consume(toolCall('c2', 'r1'));
      controller.consume({ type: 'interrupted', eventId: 'interrupt' });
      expect(controller.settleToolCall('c1')).toBe('wait');
      expect(controller.settleToolCall('c2')).toBe('closed');
      expect(controller.settleToolCall('c-never-seen')).toBe('closed');
    });

    it('reports outstanding tool calls only between the call and its result', () => {
      const controller = respondingController();
      expect(controller.hasOutstandingToolCalls()).toBe(false);
      controller.consume(toolCall('c1', 'r1'));
      expect(controller.hasOutstandingToolCalls()).toBe(true);
      controller.settleToolCall('c1');
      expect(controller.hasOutstandingToolCalls()).toBe(false);
    });

    it('bounds the ledger over a long session and drops it on reset and close', () => {
      const controller = respondingController('r0');
      controller.consume(toolCall('c0', 'r0'));
      // Far more responses than one turn can have in flight, each left with an
      // unanswered call so nothing is retired the tidy way: a session that runs
      // for an hour must not keep every response it ever saw.
      for (let index = 1; index < 80; index += 1) {
        controller.consume({ type: 'response_started', eventId: `started:${index}`, responseId: `r${index}` });
        controller.consume(toolCall(`c${index}`, `r${index}`));
      }
      // The eviction is silent, so the oldest response reads as an ordinary
      // finished turn rather than one still waiting for a continuation.
      expect(controller.responseDisposition('r0')).toBe('complete');
      expect(controller.settleToolCall('c0')).toBe('closed');
      // The newest response is untouched by the eviction and still continues.
      controller.consume({ type: 'response_done', eventId: 'done', responseId: 'r79', status: 'completed' });
      expect(controller.responseDisposition('r79')).toBe('await_tools');
      expect(controller.settleToolCall('c79')).toBe('continue');
      controller.close();
      expect(controller.settleToolCall('c78')).toBe('closed');
      expect(controller.hasOutstandingToolCalls()).toBe(false);
      controller.reset();
      expect(controller.settleToolCall('c77')).toBe('closed');
    });
  });
});
