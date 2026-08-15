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
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => undefined) }));
vi.mock('../../lib/agentLoop', () => ({
  runAgentTurn: vi.fn(() => Promise.resolve()),
  stopTurn: vi.fn(),
}));

import { TalkPanel } from './TalkPanel';
import type { CompanionConfig } from '../../lib/companionClient';
import type { TalkStatus } from '../../lib/talkClient';

const CONFIG: CompanionConfig = {
  schemaVersion: 1,
  overlayShortcut: 'CommandOrControl+Shift+Space',
  voice: {
    backend: 'local_whisper',
    whisperBinary: '/usr/local/bin/whisper',
    whisperModel: '/models/base.bin',
    providerId: null,
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
      default:
        return Promise.resolve(null);
    }
  });
  return saved;
}

beforeEach(() => {
  invoke.mockReset();
});

afterEach(() => {
  cleanup();
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
