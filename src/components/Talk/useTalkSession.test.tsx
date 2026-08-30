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

const streams: { stopped: number }[] = [];
let resumed = 0;
let sourceNodes: { connected: number; disconnected: number }[] = [];

function stubMedia() {
  streams.length = 0;
  resumed = 0;
  sourceNodes = [];
  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: {
      getUserMedia: async () => {
        const track = { stopped: 0, stop() { this.stopped += 1; } };
        streams.push(track);
        return { getTracks: () => [track] };
      },
    },
  });
  vi.stubGlobal(
    'MediaRecorder',
    class {
      static isTypeSupported = () => true;
      state = 'inactive';
      mimeType = 'audio/webm';
      start() { this.state = 'recording'; }
      stop() { this.state = 'inactive'; }
    },
  );
  vi.stubGlobal(
    'AudioContext',
    class {
      // What WebKit hands back for a context built outside a user gesture.
      state = 'suspended';
      resume() {
        this.state = 'running';
        resumed += 1;
        return Promise.resolve();
      }
      createAnalyser() {
        return { fftSize: 1_024, getFloatTimeDomainData: (buffer: Float32Array) => buffer.fill(0) };
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

  it('holds the source node, so the analyser keeps being fed', async () => {
    const { rerender } = renderHook(
      ({ enabled }: { enabled: boolean }) =>
        useTalkSession('session-1', { enabled, autoStartMode: 'continuous' }),
      { initialProps: { enabled: true } },
    );
    await waitFor(() => expect(sourceNodes).toHaveLength(1));

    rerender({ enabled: false });
    // Nothing can disconnect a node it never kept, and WebKit collects a source
    // node nothing references — leaving the analyser reading silence, the
    // meter flat, and Talk listening forever.
    await waitFor(() => expect(sourceNodes[0].disconnected).toBe(1));
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
