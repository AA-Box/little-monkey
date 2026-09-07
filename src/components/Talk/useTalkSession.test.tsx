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
vi.mock('../../lib/agentLoop', () => ({
  runAgentTurn: () => Promise.resolve(),
  stopTurn: () => undefined,
}));

import { useTalkSession } from './useTalkSession';

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
  invoke.mockImplementation((command: string) => {
    switch (command) {
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
