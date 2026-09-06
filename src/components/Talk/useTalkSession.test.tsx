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
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => undefined) }));
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
        return Promise.resolve(CONFIG);
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
