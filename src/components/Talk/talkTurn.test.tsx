// @vitest-environment jsdom
/**
 * A spoken turn, end to end, through the hook that owns it.
 *
 * These were written against the standalone Talk page. The page is gone — Talk
 * is a control in the chat composer now — but none of the claims below were
 * ever about its markup: what is spoken, what is never spoken, which device the
 * answer comes out of, and what the recognizer is primed with are properties of
 * `useTalkSession` and the engine underneath it. So they are driven here the
 * way the composer drives them: the hook, enabled, in continuous mode.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';

const invoke = vi.fn();
const runAgentTurn = vi.fn((..._args: unknown[]) => Promise.resolve());
const stopTurn = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => undefined) }));
vi.mock('../../lib/agentLoop', () => ({
  runAgentTurn: (...args: unknown[]) => runAgentTurn(...args),
  stopTurn: (...args: unknown[]) => stopTurn(...args),
}));

import { useTalkSession, type UseTalkSession } from './useTalkSession';
import type { CompanionConfig } from '../../lib/companionClient';
import type { TalkStatus } from '../../lib/talkClient';
import { useSessionStore } from '../../store/sessionStore';

const CONFIG: CompanionConfig = {
  schemaVersion: 1,
  overlayShortcut: 'CommandOrControl+Shift+Space',
  voice: {
    backend: 'local_whisper',
    whisperBinary: null,
    whisperModel: null,
    providerId: null,
    extensionId: null,
    extensionCapabilityId: null,
    ttsExtensionId: null,
    ttsExtensionCapabilityId: null,
    realtimeBackend: 'system',
    realtimeExtensionId: null,
    realtimeExtensionCapabilityId: null,
    providerModel: 'whisper-1',
    language: 'auto',
    transcriptionModel: 'base',
    ttsVoice: null,
    saveRawAudio: false,
    inputDeviceId: null,
    outputDeviceId: null,
    ttsBackend: 'system',
    vadMinSpeechMs: 180,
    vadSilenceMs: 800,
    vadMaxUtteranceMs: 90_000,
    wakePhraseEnabled: false,
    wakePhrase: 'hey little monkey',
    alwaysListening: false,
    dictationLanguage: null,
    dictationRequireOnDevice: false,
  },
  imageEndpoints: [],
};

function mock(status: Partial<TalkStatus> = {}, config: CompanionConfig = CONFIG) {
  const saved: CompanionConfig[] = [];
  invoke.mockImplementation((command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'm7_talk_status':
        return Promise.resolve({
          configured: true,
          wakePhraseEnabled: false,
          alwaysListening: false,
          backend: 'local_whisper',
          activeJobs: 0,
          activeMicrophoneGrants: 0,
          wakeWord: {
            backend: 'sherpa_onnx',
            local: true,
            runtimeVersion: '1.13.3',
            modelId: 'test-model',
            modelLicense: 'Apache-2.0',
            available: true,
            loaded: false,
            acceptingAudio: false,
            sampleRate: 16_000,
            modelBytes: 1,
            modelMemoryBytes: null,
            idleCpuPercent: null,
            averageInferenceMs: null,
            averageDetectionLatencyMs: null,
            detections: 0,
            falseTriggerReports: 0,
            droppedFrames: 0,
            lastError: null,
          },
          ...status,
        } satisfies TalkStatus);
      case 'm7_config_get':
        return Promise.resolve(saved[saved.length - 1] ?? config);
      case 'm7_config_save':
        saved.push(args?.config as CompanionConfig);
        return Promise.resolve(args?.config);
      case 'm7_talk_metric_record':
        return Promise.resolve({ metrics: [], interruptCount: 0, fallbackCount: 0 });
      case 'realtime_voice_status':
        return Promise.resolve({
          providerId: 'openai', configured: true, activeSessions: 0,
          endpoint: 'https://api.openai.com/v1/realtime/calls',
        });
      case 'm7_capture_grant':
        return Promise.resolve({
          grantId: 'grant-1',
          kind: 'microphone',
          applicationId: 'talk',
          createdAtMs: Date.now(),
          expiresAtMs: Date.now() + 60_000,
          active: true,
        });
      case 'm7_talk_transcribe':
        return Promise.resolve({ jobId: args?.jobId, text: 'what is the deploy status' });
      case 'm7_tts_synthesize':
        return Promise.resolve({
          jobId: args?.jobId,
          mediaType: 'audio/wav',
          audioBase64: btoa('spoken'),
        });
      case 'm7_wake_word_start':
        return Promise.resolve({
          sessionId: 'wake-1',
          status: { backend: 'sherpa_onnx', local: true, available: true, loaded: true },
        });
      case 'm7_wake_word_push':
        return Promise.resolve(null);
      case 'm7_wake_word_stop':
        return Promise.resolve(true);
      default:
        return Promise.resolve(null);
    }
  });
  return saved;
}

/** Every command Talk sent, in order, for asking which path it took. */
const commands = () => invoke.mock.calls.map((call) => call[0] as string);

class FakeTrack {
  stopped = 0;
  stop() {
    this.stopped += 1;
  }
}

class FakeStream {
  tracks = [new FakeTrack()];
  getTracks() {
    return this.tracks;
  }
}

/**
 * The devices jsdom does not have.
 *
 * Talk's decisions are tested through the engine's ports; what is left here is
 * the part the hook genuinely owns — opening the microphone, and handing a
 * clip to a speaker — so these stand in for the hardware and record what they
 * were asked to do.
 */
function stubMedia(options: { routing?: boolean } = {}) {
  const streams: FakeStream[] = [];
  const recorders: FakeWorklet[] = [];
  const speakers: FakeSpeaker[] = [];

  class FakeWorklet {
    port = {
      onmessage: null as ((event: MessageEvent<Float32Array>) => void) | null,
      close: () => undefined,
    };
    constructor() {
      recorders.push(this);
    }
    disconnect() {}
    emit(samples = new Float32Array(2_048).fill(0.2)) {
      this.port.onmessage?.({ data: samples } as MessageEvent<Float32Array>);
    }
  }

  class FakeSpeaker {
    currentTime = 0;
    onended: (() => void) | null = null;
    onerror: (() => void) | null = null;
    sinks: string[] = [];
    plays = 0;
    setSinkId?: (deviceId: string) => Promise<void>;
    /** Assigned per clip now that one element plays every clip. */
    src = '';
    /** Every clip this element was pointed at, in order. */
    srcs: string[] = [];
    constructor() {
      if (options.routing !== false) {
        this.setSinkId = async (deviceId) => {
          this.sinks.push(deviceId);
        };
      }
      speakers.push(this);
    }
    async play() {
      this.srcs.push(this.src);
      this.plays += 1;
      queueMicrotask(() => this.onended?.());
    }
    pause() {}
  }

  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: {
      getUserMedia: async () => {
        const stream = new FakeStream();
        streams.push(stream);
        return stream;
      },
      enumerateDevices: async () => [],
    },
  });
  vi.stubGlobal('AudioWorkletNode', FakeWorklet);
  vi.stubGlobal('Audio', FakeSpeaker);
  vi.stubGlobal(
    'AudioContext',
    class {
      sampleRate = 48_000;
      state = 'running';
      audioWorklet = { addModule: () => Promise.resolve() };
      createAnalyser() {
        return {
          fftSize: 1_024,
          getFloatTimeDomainData: (buffer: Float32Array) => buffer.fill(0),
        };
      }
      createMediaStreamSource() {
        // Held by the hook for as long as the microphone is open, and
        // disconnected when it closes — WebKit collects an unreferenced one.
        return { connect: () => undefined, disconnect: () => undefined };
      }
      close() {
        return Promise.resolve();
      }
    },
  );
  URL.createObjectURL = () => `blob:clip-${speakers.length + 1}`;
  URL.revokeObjectURL = () => undefined;
  return { streams, recorders, speakers };
}

/** The hook as the composer renders it once somebody has asked for Talk. */
function talk(sessionId: string) {
  return renderHook(() => useTalkSession(sessionId, { enabled: true, autoStartMode: 'continuous' }));
}

/** The microphone this session opened, once it is really open. */
async function listening(
  result: { current: UseTalkSession },
  media: ReturnType<typeof stubMedia>,
) {
  await waitFor(() => expect(media.recorders).toHaveLength(1));
  await waitFor(() => expect(result.current.snapshot?.capturing).toBe(true));
  const engine = result.current.sessionRef.current;
  if (!engine) throw new Error('Talk never built an engine');
  return engine;
}

/**
 * Say something into the open microphone, and stop talking.
 *
 * Continuous capture is ended by the detector, not by a key, so the utterance
 * is closed the way silence closes it. The frames carry their own timestamps —
 * the same parameter the worklet's own frames use — because the alternative is
 * a test that sleeps through a real second of `vadSilenceMs` per utterance.
 */
async function saySomething(
  result: { current: UseTalkSession },
  media: ReturnType<typeof stubMedia>,
): Promise<void> {
  const engine = await listening(result, media);
  await act(async () => {
    media.recorders[0].emit();
    const at = Date.now();
    // Past `vadMinSpeechMs`: somebody is talking.
    engine.observeLevel(0.2, at + 180);
    // Past `vadSilenceMs` of quiet after that: they stopped.
    engine.observeLevel(0, at + 1_200);
  });
}

/** A session in the real store, since Talk reads the answer from it. */
function liveSession(): string {
  return useSessionStore.getState().sessions[0].id;
}

beforeEach(() => {
  invoke.mockReset();
  runAgentTurn.mockReset();
  runAgentTurn.mockImplementation(() => Promise.resolve());
  stopTurn.mockReset();
  vi.stubGlobal('crypto', {
    ...globalThis.crypto,
    randomUUID: (() => {
      let counter = 0;
      return () => `uuid-${(counter += 1)}`;
    })(),
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('Talk — a spoken turn end to end', () => {
  it('plays the answer through the chosen output and keeps no recording of it', async () => {
    const media = stubMedia();
    mock({}, { ...CONFIG, voice: { ...CONFIG.voice, outputDeviceId: 'speaker-2' } });
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'what is the deploy status' });
      store.addMessage(sessionId, { role: 'assistant', content: 'The deploy finished. ' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    await waitFor(() => expect(media.speakers).toHaveLength(1));
    // The setting that was previously true only of the speaker test.
    expect(media.speakers[0].sinks).toEqual(['speaker-2']);
    expect(media.speakers[0].plays).toBe(1);
    // Talk transcribes through its own non-publishing path: the companion's
    // command writes the transcript, and the audio too, as artifacts.
    expect(commands()).toContain('m7_talk_transcribe');
    expect(commands()).not.toContain('m7_transcribe_audio');

    // And the answer is spoken once. The store keeps moving after a turn ends,
    // and every one of those mutations used to look like a fresh answer.
    const spoken = commands().filter((command) => command === 'm7_tts_synthesize').length;
    await act(async () => {
      useSessionStore.getState().addMessage(sessionId, { role: 'system', content: 'a later note' });
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(commands().filter((command) => command === 'm7_tts_synthesize')).toHaveLength(spoken);
  });

  it('plays on the system default when the browser cannot route to a device', async () => {
    const media = stubMedia({ routing: false });
    mock({}, { ...CONFIG, voice: { ...CONFIG.voice, outputDeviceId: 'speaker-2' } });
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      useSessionStore
        .getState()
        .addMessage(sessionId, { role: 'assistant', content: 'The deploy finished. ' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    // No `setSinkId` anywhere, and the conversation still happens.
    await waitFor(() => expect(media.speakers[0]?.plays).toBe(1));
  });

  it('never reads out the daemon placeholder, or the answer minus its first sentence', async () => {
    const media = stubMedia();
    mock();
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'what is the deploy status' });
      // The resident runner parks this in the answer's place and then replaces
      // it wholesale, which is not the same shape as streaming into it.
      store.addMessage(sessionId, {
        role: 'assistant',
        content: '⏳ Queued in the resident runner…',
      });
      await Promise.resolve();
      store.updateLastMessage(sessionId, { content: 'The deploy finished cleanly. ' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    await waitFor(() => expect(media.speakers).toHaveLength(1));
    const spoken = invoke.mock.calls
      .filter((call) => call[0] === 'm7_tts_synthesize')
      .map((call) => (call[1] as { text: string }).text);
    expect(spoken).toEqual(['The deploy finished cleanly.']);
  });

  it('never narrates the run: no queue, no progress, no tool status', async () => {
    const media = stubMedia();
    mock();
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'what is the deploy status' });
      // Every status `projectDaemonTurnEvents` writes lands in the answer's own
      // message, one after another. `started` reaches nearly every turn, which
      // is why "Resident agent is working…" was read out before every answer.
      store.addMessage(sessionId, { role: 'assistant', content: '⏳ Queued in the resident runner…' });
      await Promise.resolve();
      store.updateLastMessage(sessionId, { content: '⏳ Resident agent is working…' });
      await Promise.resolve();
      store.updateLastMessage(sessionId, { content: '⏳ Preparing read_file…' });
      await Promise.resolve();
      store.updateLastMessage(sessionId, { content: 'The deploy finished cleanly. ' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    await waitFor(() => expect(media.speakers).toHaveLength(1));
    const spoken = invoke.mock.calls
      .filter((call) => call[0] === 'm7_tts_synthesize')
      .map((call) => (call[1] as { text: string }).text);
    expect(spoken).toEqual(['The deploy finished cleanly.']);
  });

  it('offers the conversation to the recognizer, minus the plumbing', async () => {
    const media = stubMedia();
    mock();
    const sessionId = liveSession();
    await act(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'I live in Sundbyberg' });
      store.addMessage(sessionId, { role: 'assistant', content: '⏳ Resident agent is working…' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    // The recorder stops, then the audio is transcribed a tick later.
    await waitFor(() =>
      expect(invoke.mock.calls.some((entry) => entry[0] === 'm7_talk_transcribe')).toBe(true),
    );
    const call = invoke.mock.calls.find((entry) => entry[0] === 'm7_talk_transcribe');
    const context = (call?.[1] as { context: string | null } | undefined)?.context ?? '';
    // A name already on screen is spelled for the decoder, so the next
    // utterance is not decoded against a vocabulary that lacks it.
    expect(context).toContain('Sundbyberg');
    // Progress text is not conversation; priming with it teaches the decoder
    // the plumbing's words.
    expect(context).not.toContain('Resident agent');
  });

  it('speaks the answer of a turn that called a tool', async () => {
    const media = stubMedia();
    mock();
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'what is the deploy status' });
      // A tool round writes its own assistant message first, and that one is
      // empty. Watching the turn's *first* assistant message left every
      // tool-using turn — which is most of them — silent.
      store.addMessage(sessionId, {
        role: 'assistant',
        content: '',
        tool_calls: [{ id: 'call-1', type: 'function', function: { name: 'read_file', arguments: '{}' } }],
      });
      store.addMessage(sessionId, { role: 'tool', tool_call_id: 'call-1', content: 'deploy.log' });
      await Promise.resolve();
      store.addMessage(sessionId, { role: 'assistant', content: 'The deploy finished cleanly. ' });
    });
    const { result } = talk(sessionId);

    await saySomething(result, media);
    await waitFor(() => expect(media.speakers).toHaveLength(1));
    const spoken = invoke.mock.calls
      .filter((call) => call[0] === 'm7_tts_synthesize')
      .map((call) => (call[1] as { text: string }).text);
    expect(spoken).toEqual(['The deploy finished cleanly.']);
  });

  it('says nothing about a message typed in the composer', async () => {
    const media = stubMedia();
    mock();
    const sessionId = liveSession();
    const { result } = talk(sessionId);
    // Listening, and nobody has said anything into it.
    await listening(result, media);

    await act(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'typed, not spoken' });
      store.addMessage(sessionId, { role: 'assistant', content: 'An answer to the typed one. ' });
    });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(commands()).not.toContain('m7_tts_synthesize');
    // The element exists from the moment the microphone opened — that is where
    // the output device is chosen, inside the gesture WebKit requires. What
    // matters is that nothing was ever played through it.
    expect(media.speakers.flatMap((speaker) => speaker.srcs ?? [])).toHaveLength(0);
    expect(media.speakers.every((speaker) => speaker.plays === 0)).toBe(true);
  });
});
