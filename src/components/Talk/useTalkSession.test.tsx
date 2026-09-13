// @vitest-environment jsdom
/**
 * The hook's own claim, which neither surface above it can make alone.
 *
 * `TalkPanel.test.tsx` covers a conversation end to end. What is left here is
 * the gate the chat composer depends on: a ChatWindow renders this hook for
 * every open session, and until somebody presses Talk it must cost nothing —
 * no IPC, no engine, and above all no microphone. Then, when it is enabled, it
 * must actually open one without waiting for a second press.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, renderHook, waitFor } from '@testing-library/react';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
/**
 * The real `listen`, minus Tauri: a saved configuration reaches every window as
 * an event, and the regression below turns Always Listening off the only way an
 * operator can — by saving it somewhere else and letting that event arrive.
 */
// A hoisted function, not a `const` array: stores subscribe to their own
// events while this module is still being imported, which is before any
// top-level binding here is initialized.
function eventListeners(): Array<{ name: string; handler: () => void }> {
  const store = globalThis as { __talkEventListeners?: Array<{ name: string; handler: () => void }> };
  return (store.__talkEventListeners ??= []);
}
vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, handler: () => void) => {
    const entry = { name, handler };
    eventListeners().push(entry);
    return Promise.resolve(() => {
      const index = eventListeners().indexOf(entry);
      if (index >= 0) eventListeners().splice(index, 1);
    });
  },
}));

function emit(name: string): void {
  for (const listener of [...eventListeners()]) if (listener.name === name) listener.handler();
}
/** A spy, because the route regressions below are all questions about how many
 * times one physical utterance became a turn in the conversation. */
const runAgentTurn = vi.fn(async (..._args: unknown[]) => undefined);
vi.mock('../../lib/agentLoop', () => ({
  runAgentTurn: (...args: unknown[]) => runAgentTurn(...args),
  stopTurn: () => undefined,
}));

import { useTalkSession } from './useTalkSession';
import type { VoiceRouteEvent, VoiceRouteRecord } from '../../lib/daemonClient';

const CONFIG = {
  schemaVersion: 1,
  overlayShortcut: 'CommandOrControl+Shift+Space',
  voice: {
    backend: 'local_whisper',
    vadMinSpeechMs: 180,
    vadSilenceMs: 800,
    vadMaxUtteranceMs: 90_000,
    wakePhraseEnabled: false,
    wakePhrase: 'hey little monkey',
    alwaysListening: false,
    inputDeviceId: null,
    outputDeviceId: null,
  },
};

/**
 * What `m7_config_get` answers right now, which is not a constant: the whole
 * point of the always-listening regression is that this changes underneath a
 * running session, exactly as a save in Settings changes it.
 */
let voice: Record<string, unknown> = { ...CONFIG.voice };

/**
 * The daemon's VoiceRoute half: the record every route command answers with,
 * and the append-only coordination log the hook polls with a cursor.
 */
let voiceRoute: VoiceRouteRecord | null = null;
let voiceRouteLog: VoiceRouteEvent[] = [];
/** Whether `voice_route_activate` refuses, the way a paired device that never
 * acknowledged its command makes it refuse. */
let activationFails = false;
/** How many local microphone tracks had already been stopped at the moment the
 * daemon was asked to give capture to the paired device. */
let stoppedTracksWhenActivated: number | null = null;

function pairedRoute(generation: number): VoiceRouteRecord {
  return {
    session_id: 'session-1',
    route_id: 'route-1',
    generation,
    engine: 'pipeline',
    input_endpoint: 'paired:phone-1:input',
    output_endpoint: 'local:output:default',
    state: 'active',
    input_command_id: null,
    output_command_id: null,
    created_at_ms: 1,
    updated_at_ms: 1,
  };
}

function transcript(eventId: number, generation: number, text: string): VoiceRouteEvent {
  return {
    event_id: eventId,
    session_id: 'session-1',
    generation,
    kind: 'input_transcript',
    payload: { text, turn_id: `turn-${eventId}` },
    created_at_ms: 2,
  };
}

interface StubTrack {
  stopped: number;
  listeners: Record<string, Array<() => void>>;
  stop(): void;
  addEventListener(kind: string, listener: () => void): void;
}

const streams: StubTrack[] = [];
/** The operating system's answer to the microphone prompt, per test. */
let microphoneGrant: 'granted' | 'denied' = 'granted';
let resumed = 0;
let sourceNodes: { connected: number; disconnected: number }[] = [];
let workletNodes: Array<{ port: { onmessage: ((event: MessageEvent<Float32Array>) => void) | null; close(): void }; disconnect(): void }> = [];

function stubMedia() {
  streams.length = 0;
  microphoneGrant = 'granted';
  resumed = 0;
  sourceNodes = [];
  workletNodes = [];
  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: {
      getUserMedia: async () => {
        if (microphoneGrant === 'denied') {
          // What every browser throws when the operator says no, or when the
          // platform has already said no on their behalf.
          const refusal = new Error('Permission denied');
          refusal.name = 'NotAllowedError';
          throw refusal;
        }
        const track: StubTrack = {
          stopped: 0,
          listeners: {},
          stop() { this.stopped += 1; },
          addEventListener(kind: string, listener: () => void) {
            (this.listeners[kind] ??= []).push(listener);
          },
        };
        streams.push(track);
        return { getTracks: () => [track] };
      },
    },
  });
  vi.stubGlobal('AudioWorkletNode', class {
    port = { onmessage: null as ((event: MessageEvent<Float32Array>) => void) | null, close() {} };
    constructor() { workletNodes.push(this); }
    disconnect() {}
  });
  vi.stubGlobal(
    'AudioContext',
    class {
      sampleRate = 48_000;
      audioWorklet = { addModule: () => Promise.resolve() };
      // What WebKit hands back for a context built outside a user gesture.
      state = 'suspended';
      resume() {
        this.state = 'running';
        resumed += 1;
        return Promise.resolve();
      }
      createMediaStreamSource() {
        const node = {
          connected: 0,
          disconnected: 0,
          connect() { this.connected += 1; },
          disconnect() { this.disconnected += 1; },
        };
        sourceNodes.push(node);
        return node;
      }
      close() { return Promise.resolve(); }
    },
  );
}

beforeEach(() => {
  invoke.mockReset();
  invoke.mockImplementation((command: string, args?: unknown) => {
    const input = (args ?? {}) as { after?: number };
    switch (command) {
      case 'voice_route_get':
      case 'voice_route_deactivate':
        return Promise.resolve(voiceRoute);
      case 'voice_route_activate':
        stoppedTracksWhenActivated = streams.reduce((total, track) => total + track.stopped, 0);
        return activationFails
          ? Promise.reject(new Error('A paired VoiceRoute endpoint did not become ready'))
          : Promise.resolve(voiceRoute);
      case 'voice_route_events':
        // Strictly after the cursor, as the daemon answers. A host that rewinds
        // its cursor therefore sees the same event a second time, which is what
        // makes a replayed turn observable at all.
        return Promise.resolve(voiceRouteLog.filter((event) => event.event_id > (input.after ?? 0)));
      case 'voice_route_emit':
        return Promise.resolve({ event_id: 900 + voiceRouteLog.length });
      case 'm7_talk_status':
        return Promise.resolve({
          configured: true,
          wakePhraseEnabled: false,
          alwaysListening: false,
          backend: 'local_whisper',
          activeJobs: 0,
          activeMicrophoneGrants: 0,
        });
      case 'm7_config_get':
        return Promise.resolve({ ...CONFIG, voice });
      case 'm7_capture_grant':
        return Promise.resolve({
          grantId: 'grant-1',
          kind: 'microphone',
          applicationId: 'talk',
          createdAtMs: Date.now(),
          expiresAtMs: Date.now() + 60_000,
          active: true,
        });
      case 'm7_wake_word_start':
        return Promise.resolve({
          sessionId: 'wake-1',
          status: { backend: 'sherpa_onnx', local: true, available: true, loaded: true },
        });
      case 'm7_wake_word_stop':
        return Promise.resolve(true);
      default:
        return Promise.resolve(null);
    }
  });
  stubMedia();
  eventListeners().length = 0;
  voice = { ...CONFIG.voice };
  runAgentTurn.mockClear();
  voiceRoute = null;
  voiceRouteLog = [];
  activationFails = false;
  stoppedTracksWhenActivated = null;
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('useTalkSession', () => {
  it('costs nothing until it is enabled', async () => {
    renderHook(() => useTalkSession('session-1', { enabled: false, autoStartMode: 'continuous' }));
    // Long enough for any effect that was going to fire to have fired.
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(invoke).not.toHaveBeenCalled();
    expect(streams).toHaveLength(0);
  });

  it('opens the microphone in continuous mode as soon as it is', async () => {
    const { result } = renderHook(() =>
      useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous' }),
    );
    await waitFor(() => expect(streams).toHaveLength(1));
    await waitFor(() => expect(result.current.snapshot?.capturing).toBe(true));
    expect(result.current.mode).toBe('continuous');
  });

  it('resumes the audio context, so the detector hears something', async () => {
    renderHook(() => useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous' }));
    await waitFor(() => expect(streams).toHaveLength(1));
    // A suspended context reads pure silence: the meter sits at zero, the
    // utterance never ends, and Talk listens forever without answering.
    await waitFor(() => expect(resumed).toBe(1));
  });

  it('holds the source node, so the worklet keeps being fed', async () => {
    const { rerender } = renderHook(
      ({ enabled }: { enabled: boolean }) =>
        useTalkSession('session-1', { enabled, autoStartMode: 'continuous' }),
      { initialProps: { enabled: true } },
    );
    await waitFor(() => expect(sourceNodes).toHaveLength(1));

    rerender({ enabled: false });
    // Nothing can disconnect a node it never kept, and WebKit collects a source
    // node nothing references — leaving the worklet without PCM, the
    // meter flat, and Talk listening forever.
    await waitFor(() => expect(sourceNodes[0].disconnected).toBe(1));
  });

  /**
   * The one step of the acceptance script no test can perform is a human
   * clicking the operating system's microphone prompt. What the hook does with
   * each of that prompt's two answers is not, and this is it: a refusal is
   * reported and nothing claims to be listening.
   */
  it('reports a refused microphone instead of claiming to listen', async () => {
    microphoneGrant = 'denied';
    const { result } = renderHook(() =>
      useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous' }),
    );
    await waitFor(() => expect(result.current.snapshot?.error ?? result.current.setupError).toBeTruthy());
    expect(streams).toHaveLength(0);
    expect(result.current.snapshot?.capturing).not.toBe(true);
    expect(result.current.snapshot?.state).not.toBe('armed');
    expect(invoke).not.toHaveBeenCalledWith('m7_wake_word_start', expect.anything());
  });

  /**
   * A grant can be taken back while Talk holds it — the operator revokes it in
   * system settings, or the device disappears. The track ends, and an engine
   * that kept saying "listening" would be lying about an open microphone.
   */
  it('fails closed when the grant is revoked mid-session', async () => {
    const { result } = renderHook(() =>
      useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous' }),
    );
    await waitFor(() => expect(streams).toHaveLength(1));
    await waitFor(() => expect(result.current.snapshot?.capturing).toBe(true));

    const ended = streams[0].listeners.ended ?? [];
    expect(ended.length).toBeGreaterThan(0);
    for (const listener of ended) listener();

    await waitFor(() => expect(result.current.snapshot?.state).toBe('error'));
    expect(result.current.snapshot?.capturing).toBe(false);
  });

  /**
   * The acceptance script's own step, performed the way an operator performs it.
   *
   * Always Listening is the only reason Talk opens a microphone nobody pressed
   * anything for, so its switch has to close that microphone. Nothing here
   * calls `stop()`, and nothing unmounts the surface: the configuration is
   * saved somewhere else — Settings, the other panel, another window — and the
   * only thing that reaches this session is the event saying so. A test that
   * stopped the session itself would prove `stop()` works, which was never the
   * question.
   */
  it('closes the microphone when Always Listening alone is turned off', async () => {
    voice = { ...CONFIG.voice, wakePhraseEnabled: true, alwaysListening: true };
    const { result } = renderHook(() => useTalkSession('session-1'));

    await waitFor(() => expect(streams).toHaveLength(1));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('m7_wake_word_start', expect.anything()),
    );
    await waitFor(() => expect(result.current.snapshot?.state).toBe('armed'));
    expect(result.current.snapshot?.awaitingWakeWord).toBe(true);

    // The operator turns it off in Settings. This is the entire act.
    voice = { ...voice, alwaysListening: false, wakePhraseEnabled: false };
    emit('m7://config-changed');

    await waitFor(() => expect(streams[0].stopped).toBeGreaterThan(0));
    await waitFor(() => expect(result.current.snapshot?.state).toBe('off'));
    // The native generation is closed, not merely unsubscribed from, and the
    // capture grant it was pushing under is handed back.
    expect(invoke).toHaveBeenCalledWith('m7_wake_word_stop', expect.anything());
    expect(invoke).toHaveBeenCalledWith('m7_capture_revoke', expect.anything());
    expect(sourceNodes[0].disconnected).toBe(1);
    expect(result.current.snapshot?.capturing).toBe(false);
    // And the banner above it is re-read, so nothing on screen still claims the
    // microphone this just closed is active.
    await waitFor(() =>
      expect(invoke.mock.calls.filter(([command]) => command === 'm7_talk_status').length)
        .toBeGreaterThan(1),
    );
  });

  /** Saving an output device is not a reason to end a conversation. */
  it('keeps listening when a save leaves Always Listening on', async () => {
    voice = { ...CONFIG.voice, wakePhraseEnabled: true, alwaysListening: true };
    const { result } = renderHook(() => useTalkSession('session-1'));
    await waitFor(() => expect(result.current.snapshot?.state).toBe('armed'));

    voice = { ...voice, outputDeviceId: 'headphones' };
    emit('m7://config-changed');

    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(streams[0].stopped).toBe(0);
    expect(result.current.snapshot?.state).toBe('armed');
  });

  /**
   * A press is not the setting. Talk opened from the composer was asked for
   * directly, and revoking a setting the operator did not use to open it does
   * not retract that ask — the session keeps the microphone it was given and
   * the next one is built from the new configuration.
   */
  it('leaves a session somebody pressed Talk for running', async () => {
    voice = { ...CONFIG.voice, wakePhraseEnabled: true, alwaysListening: true };
    const { result } = renderHook(() =>
      useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous' }),
    );
    await waitFor(() => expect(streams).toHaveLength(1));

    voice = { ...voice, alwaysListening: false, wakePhraseEnabled: false };
    emit('m7://config-changed');

    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(streams[0].stopped).toBe(0);
    expect(result.current.snapshot?.state).not.toBe('off');
  });

  it('closes the microphone when it is disabled again', async () => {
    const { rerender } = renderHook(
      ({ enabled }: { enabled: boolean }) =>
        useTalkSession('session-1', { enabled, autoStartMode: 'continuous' }),
      { initialProps: { enabled: true } },
    );
    await waitFor(() => expect(streams).toHaveLength(1));
    rerender({ enabled: false });
    await waitFor(() => expect(streams[0].stopped).toBeGreaterThan(0));
  });
});

/**
 * One physical utterance is one turn in the conversation, however many times
 * the selector re-reads the route it was captured under.
 *
 * `VoiceRouteSelector` calls `onRoute` on every refresh — a headset plugged in
 * anywhere on the machine is enough — and hands back a freshly deserialised
 * record for a route that has not moved. Keying the event cursor off that
 * object rewound it to zero on each of those, and the poll then re-submitted
 * every transcript of the live generation as another user turn: one sentence
 * spoken into the phone, answered again and again for as long as the panel
 * stayed open.
 */
describe('a paired microphone routed into an ordinary conversation', () => {
  /** Local Talk running, then the paired phone selected: the live handoff. */
  async function handOffToPhone(generation = 2) {
    const view = renderHook(
      ({ route }: { route: VoiceRouteRecord | null }) =>
        useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous', route }),
      { initialProps: { route: null as VoiceRouteRecord | null } },
    );
    await waitFor(() => expect(streams).toHaveLength(1));
    await waitFor(() => expect(view.result.current.snapshot?.capturing).toBe(true));
    voiceRoute = pairedRoute(generation);
    view.rerender({ route: voiceRoute });
    await waitFor(() => expect(stoppedTracksWhenActivated).not.toBeNull());
    return view;
  }

  it('does not replay a transcript when the same route is handed back again', async () => {
    const view = await handOffToPhone();
    voiceRouteLog.push(transcript(4, 2, 'what is on my calendar'));
    await waitFor(() => expect(runAgentTurn).toHaveBeenCalledTimes(1));
    expect(runAgentTurn.mock.calls[0][1]).toBe('what is on my calendar');

    // Exactly what a refresh produces: the same route, deserialised again.
    view.rerender({ route: { ...pairedRoute(2) } });
    view.rerender({ route: { ...pairedRoute(2) } });
    // Several poll cycles — the poll runs every 180ms — with nothing new to
    // find. The question is whether it re-finds what it already answered.
    await new Promise((resolve) => setTimeout(resolve, 500));
    expect(runAgentTurn).toHaveBeenCalledTimes(1);
  });

  it('accepts the next transcript after the route really moves to a new generation', async () => {
    const view = await handOffToPhone();
    voiceRouteLog.push(transcript(4, 2, 'what is on my calendar'));
    await waitFor(() => expect(runAgentTurn).toHaveBeenCalledTimes(1));

    // A real move: the daemon retired the old capture owner and minted a new
    // generation. Its transcripts are new utterances and must be answered.
    voiceRoute = pairedRoute(3);
    view.rerender({ route: voiceRoute });
    voiceRouteLog.push(transcript(9, 3, 'and tomorrow'));
    await waitFor(() => expect(runAgentTurn).toHaveBeenCalledTimes(2));
    expect(runAgentTurn.mock.calls[1][1]).toBe('and tomorrow');
  });

  /**
   * The spec's two-microphone rule, checked at the only moment it can be
   * violated. The daemon has already retired the previous capture owner by the
   * time this record arrives; if the webview still held its own microphone open
   * while asking the phone to start recording, both would be recording this
   * conversation at once.
   */
  it('closes local capture before the paired microphone is asked to own it', async () => {
    await handOffToPhone();
    expect(stoppedTracksWhenActivated).toBeGreaterThan(0);
    expect(streams).toHaveLength(1);
    expect(invoke).toHaveBeenCalledWith('voice_route_activate', expect.anything());
  });

  /**
   * Closing local capture first is only safe while a failed handoff gives it
   * back. `move_route` restores the previous endpoints under a fresh generation
   * when activation fails, and a conversation left with neither microphone —
   * the phone refused, the laptop already closed — is the worse outcome of the
   * two this ordering trades between.
   */
  it('brings local capture back when the paired activation fails', async () => {
    const view = renderHook(
      ({ route }: { route: VoiceRouteRecord | null }) =>
        useTalkSession('session-1', { enabled: true, autoStartMode: 'continuous', route }),
      { initialProps: { route: null as VoiceRouteRecord | null } },
    );
    await waitFor(() => expect(streams).toHaveLength(1));
    await waitFor(() => expect(view.result.current.snapshot?.capturing).toBe(true));

    activationFails = true;
    const restored: VoiceRouteRecord = {
      ...pairedRoute(3),
      input_endpoint: 'local:input:default',
    };
    view.rerender({ route: pairedRoute(2) });
    // The daemon's authoritative record after its own rollback.
    voiceRoute = restored;

    await waitFor(() => expect(view.result.current.setupError).toBeTruthy());
    await waitFor(() => expect(streams).toHaveLength(2));
    expect(streams[1].stopped).toBe(0);
  });
});
