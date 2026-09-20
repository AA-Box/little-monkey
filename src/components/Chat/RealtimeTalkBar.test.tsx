// @vitest-environment jsdom
/**
 * The realtime engine's own gates, now that the page that held them is gone.
 *
 * The privacy consent gate is the one that matters: audio leaving the machine
 * for OpenAI must still require a deliberate act after Talk is pressed, not
 * follow from pressing it. The session hook is stubbed — this is about the
 * gates, not about WebRTC.
 */
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

const session = {
  state: 'idle' as string,
  inputTranscript: '',
  outputTranscript: '',
  awaitingApproval: false,
  error: null as string | null,
  start: vi.fn(async () => {}),
  stop: vi.fn(async () => {}),
  interrupt: vi.fn(async () => {}),
  startManualTurn: vi.fn(async () => {}),
  finishManualTurn: vi.fn(async () => {}),
};
vi.mock('../Talk/useRealtimeVoiceSession', () => ({
  useRealtimeVoiceSession: () => session,
}));

let configured = true;
vi.mock('../../lib/companionClient', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../../lib/companionClient')>()),
  realtimeVoiceClient: { status: async () => ({ configured }) },
}));

import { RealtimeTalkBar } from './RealtimeTalkBar';
import type { VoiceConfig } from '../../lib/companionClient';

afterEach(() => {
  cleanup();
  session.state = 'idle';
  session.error = null;
  configured = true;
  vi.clearAllMocks();
});

const VOICE = { engineKind: 'realtime', realtimeModel: 'gpt-realtime-2.1', realtimeVoice: 'marin' } as unknown as VoiceConfig;

function show(voice: Partial<VoiceConfig> = {}) {
  render(
    <RealtimeTalkBar
      sessionId="session-1"
      voice={{ ...VOICE, ...voice } as VoiceConfig}
      route={null}
      onOpenVoiceSettings={vi.fn()}
      onEnd={vi.fn()}
    />,
  );
}

describe('realtime Talk in the composer', () => {
  it('will not connect until the operator accepts what leaves the machine', async () => {
    show();
    const start = await screen.findByRole('button', { name: /start realtime talk/i });
    expect((start as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByRole('checkbox'));
    await waitFor(() => expect((screen.getByRole('button', { name: /start realtime talk/i }) as HTMLButtonElement).disabled).toBe(false));

    fireEvent.click(screen.getByRole('button', { name: /start realtime talk/i }));
    expect(session.start).toHaveBeenCalled();
  });

  it('refuses to connect with no key in the keychain, and says so', async () => {
    configured = false;
    show();
    await waitFor(() => expect(screen.getByRole('alert').textContent).toMatch(/OpenAI API key/i));
    fireEvent.click(screen.getByRole('checkbox'));
    // Consent alone is not enough: there is still no provider to connect to.
    expect((screen.getByRole('button', { name: /start realtime talk/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('offers hold-to-talk only when turn detection is manual', async () => {
    show({ realtimeTurnDetection: 'semantic_vad' } as Partial<VoiceConfig>);
    await screen.findByRole('button', { name: /start realtime talk/i });
    expect(screen.queryByRole('button', { name: /hold to talk/i })).toBeNull();

    cleanup();
    show({ realtimeTurnDetection: 'manual' } as Partial<VoiceConfig>);
    expect(await screen.findByRole('button', { name: /hold to talk/i })).toBeTruthy();
  });

  it('shows the consent gate again once a session has ended', async () => {
    session.state = 'listening';
    show();
    await waitFor(() => expect(screen.queryByRole('checkbox')).toBeNull());
    cleanup();

    session.state = 'closed';
    show();
    expect(await screen.findByRole('checkbox')).toBeTruthy();
  });
});
