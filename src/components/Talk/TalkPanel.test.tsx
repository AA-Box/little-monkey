// @vitest-environment jsdom
/**
 * The Talk surface, driven the way an operator drives it.
 *
 * The claims worth a test here are the ones a screenshot cannot make. A machine
 * that cannot transcribe must say so instead of offering a Start button that
 * fails silently. A machine that is listening continuously must be impossible
 * to miss, and must be stoppable from the surface that admits it. And Talk has
 * to be a way back to typing rather than a mode you get stuck in.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

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

import { TalkPanel } from './TalkPanel';
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
          ...status,
        } satisfies TalkStatus);
      case 'm7_config_get':
        return Promise.resolve(saved[saved.length - 1] ?? config);
      case 'm7_config_save':
        saved.push(args?.config as CompanionConfig);
        return Promise.resolve(args?.config);
      case 'm7_talk_metric_record':
        return Promise.resolve({ metrics: [], interruptCount: 0, fallbackCount: 0 });
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
      default:
        return Promise.resolve(null);
    }
  });
  return saved;
}

/** Every command the panel sent, in order, for asking which path it took. */
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
 * the part the panel genuinely owns — opening the microphone, and handing a
 * clip to a speaker — so these stand in for the hardware and record what they
 * were asked to do.
 */
function stubMedia(options: { routing?: boolean } = {}) {
  const streams: FakeStream[] = [];
  const recorders: FakeRecorder[] = [];
  const speakers: FakeSpeaker[] = [];

  class FakeRecorder {
    static isTypeSupported = () => true;
    state = 'inactive';
    mimeType = 'audio/webm';
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    constructor(_stream: unknown, init?: { mimeType?: string }) {
      if (init?.mimeType) this.mimeType = init.mimeType;
      recorders.push(this);
    }
    start() {
      this.state = 'recording';
      this.ondataavailable?.({ data: new Blob(['pretend-audio']) });
    }
    stop() {
      this.state = 'inactive';
      this.onstop?.();
    }
  }

  class FakeSpeaker {
    currentTime = 0;
    onended: (() => void) | null = null;
    onerror: (() => void) | null = null;
    sinks: string[] = [];
    plays = 0;
    setSinkId?: (deviceId: string) => Promise<void>;
    constructor(readonly src: string) {
      if (options.routing !== false) {
        this.setSinkId = async (deviceId) => {
          this.sinks.push(deviceId);
        };
      }
      speakers.push(this);
    }
    async play() {
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
  vi.stubGlobal('MediaRecorder', FakeRecorder);
  vi.stubGlobal('Audio', FakeSpeaker);
  vi.stubGlobal(
    'AudioContext',
    class {
      createAnalyser() {
        return {
          fftSize: 1_024,
          getFloatTimeDomainData: (buffer: Float32Array) => buffer.fill(0),
        };
      }
      createMediaStreamSource() {
        return { connect: () => undefined };
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

/** Hold the push-to-talk control down, say something, and let go. */
async function saySomething(media: ReturnType<typeof stubMedia>): Promise<void> {
  const start = await screen.findByRole('button', { name: /start talk/i });
  await waitFor(() => expect((start as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(start);
  const hold = await screen.findByRole('button', { name: /hold to talk/i });
  fireEvent.keyDown(hold, { key: ' ' });
  await waitFor(() => expect(media.recorders).toHaveLength(1));
  fireEvent.keyUp(hold, { key: ' ' });
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

describe('TalkPanel', () => {
  it('refuses to start when nothing can transcribe, and points at the fix', async () => {
    mock({ configured: false });
    const openSettings = vi.fn();
    render(
      <TalkPanel sessionId="session-1" onClose={vi.fn()} onOpenVoiceSettings={openSettings} />,
    );

    const warning = await screen.findByRole('alert');
    expect(warning.textContent).toContain('No transcription backend is configured');
    expect((screen.getByRole('button', { name: /start talk/i }) as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByRole('button', { name: /open voice settings/i }));
    expect(openSettings).toHaveBeenCalled();
  });

  it('offers Start when a backend is configured, and shows the state out loud', async () => {
    mock();
    render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);

    await waitFor(() =>
      expect(
        (screen.getByRole('button', { name: /start talk/i }) as HTMLButtonElement).disabled,
      ).toBe(false),
    );
    // The state is announced, not only coloured — the dot alone is unreadable
    // to a screen reader and to anyone who cannot see it.
    expect(screen.getByRole('status').textContent).toContain('Not listening');
    expect(screen.getByRole('meter', { name: /microphone level/i })).toBeTruthy();
  });

  it('makes always-listening impossible to miss and stoppable from here', async () => {
    const saved = mock({ alwaysListening: true, wakePhraseEnabled: true });
    render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);

    const banner = await screen.findByText(/Always-listening is on/i);
    expect(banner).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: /stop listening/i }));
    await waitFor(() => expect(saved).toHaveLength(1));
    // Both settings go off together: leaving the phrase armed would re-arm
    // continuous listening on the next start.
    expect(saved[0].voice.alwaysListening).toBe(false);
    expect(saved[0].voice.wakePhraseEnabled).toBe(false);
  });

  it('is a way back to typing rather than a mode you get stuck in', async () => {
    mock();
    const returnToChat = vi.fn();
    const close = vi.fn();
    render(
      <TalkPanel sessionId="session-1" onClose={close} onReturnToChat={returnToChat} />,
    );

    fireEvent.click(await screen.findByRole('button', { name: /back to typing/i }));
    expect(returnToChat).toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: /close talk/i }));
    expect(close).toHaveBeenCalled();
  });

  it('offers Continuous as a toggle and keeps Stop inert until there is something to stop', async () => {
    mock();
    render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);

    const continuous = await screen.findByRole('checkbox', { name: /continuous/i });
    expect((continuous as HTMLInputElement).checked).toBe(false);
    // Push-to-talk is the default, so its control is the one on screen.
    expect(screen.getByRole('button', { name: /hold to talk/i })).toBeTruthy();

    fireEvent.click(continuous);
    expect((continuous as HTMLInputElement).checked).toBe(true);
    // In Continuous there is no hold control — the microphone is already open.
    expect(screen.queryByRole('button', { name: /hold to talk/i })).toBeNull();

    expect((screen.getByRole('button', { name: /^stop$/i }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('TalkPanel — always listening', () => {
  const listening: CompanionConfig = {
    ...CONFIG,
    voice: { ...CONFIG.voice, wakePhraseEnabled: true, alwaysListening: true },
  };

  it('opens no microphone until the operator starts Talk', async () => {
    const media = stubMedia();
    mock();
    render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);

    await screen.findByRole('button', { name: /start talk/i });
    await new Promise((resolve) => setTimeout(resolve, 0));
    // The setting is off, so the surface being open is not consent to listen.
    expect(media.streams).toHaveLength(0);
    expect(screen.getByRole('status').textContent).toContain('Not listening');
  });

  it('captures as soon as it is opened when always-listening is on', async () => {
    const media = stubMedia();
    mock({ alwaysListening: true, wakePhraseEnabled: true }, listening);
    render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);

    // Nobody pressed Start. That is the whole claim the banner makes.
    await waitFor(() => expect(media.streams).toHaveLength(1));
    // The first `status` is the header's; the second is the banner that admits
    // what is going on.
    await waitFor(() =>
      expect(screen.getAllByRole('status')[0].textContent).toContain('waiting for the wake phrase'),
    );
    expect((await screen.findByRole('checkbox', { name: /continuous/i }) as HTMLInputElement).checked).toBe(true);
  });

  it('closes the microphone when Talk closes', async () => {
    const media = stubMedia();
    mock({ alwaysListening: true, wakePhraseEnabled: true }, listening);
    const { unmount } = render(<TalkPanel sessionId="session-1" onClose={vi.fn()} />);
    await waitFor(() => expect(media.streams).toHaveLength(1));

    unmount();
    // Foreground only: there is no listening left behind this surface.
    await waitFor(() => expect(media.streams[0].tracks[0].stopped).toBe(1));
  });
});

describe('TalkPanel — a spoken turn end to end', () => {
  it('plays the answer through the chosen output and keeps no recording of it', async () => {
    const media = stubMedia();
    mock({}, { ...CONFIG, voice: { ...CONFIG.voice, outputDeviceId: 'speaker-2' } });
    const sessionId = liveSession();
    runAgentTurn.mockImplementation(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'what is the deploy status' });
      store.addMessage(sessionId, { role: 'assistant', content: 'The deploy finished. ' });
    });
    render(<TalkPanel sessionId={sessionId} onClose={vi.fn()} />);

    await saySomething(media);
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
    render(<TalkPanel sessionId={sessionId} onClose={vi.fn()} />);

    await saySomething(media);
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
    render(<TalkPanel sessionId={sessionId} onClose={vi.fn()} />);

    await saySomething(media);
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
    render(<TalkPanel sessionId={sessionId} onClose={vi.fn()} />);
    await screen.findByRole('button', { name: /start talk/i });

    await act(async () => {
      const store = useSessionStore.getState();
      store.addMessage(sessionId, { role: 'user', content: 'typed, not spoken' });
      store.addMessage(sessionId, { role: 'assistant', content: 'An answer to the typed one. ' });
    });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(commands()).not.toContain('m7_tts_synthesize');
    expect(media.speakers).toHaveLength(0);
  });
});
