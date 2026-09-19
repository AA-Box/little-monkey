// @vitest-environment jsdom
/**
 * The controls that used to exist only on the Talk page.
 *
 * Each test here is a capability that had no home in the composer before Talk
 * moved into it, so each one is also the check that the move did not quietly
 * drop something. The hook arrives as a prop, which is what lets these run
 * without stubbing `getUserMedia` or mounting a conversation.
 */
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: () => Promise.resolve(null),
  isTauri: () => false,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => undefined) }));

import { TalkMenu } from './TalkMenu';
import type { UseTalkSession } from '../Talk/useTalkSession';
import type { TalkStatus } from '../../lib/talkClient';

afterEach(cleanup);

function talkStub(overrides: Partial<UseTalkSession> = {}): UseTalkSession {
  return {
    snapshot: null,
    status: null,
    setStatus: vi.fn(),
    mode: 'continuous',
    setMode: vi.fn(),
    setupError: null,
    setSetupError: vi.fn(),
    start: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
    sessionRef: { current: null },
    ...overrides,
  } as UseTalkSession;
}

function open(props: Partial<Parameters<typeof TalkMenu>[0]> = {}) {
  const onModeChange = vi.fn();
  const onOpenVoiceSettings = vi.fn();
  render(
    <TalkMenu
      sessionId="session-1"
      engine="pipeline"
      talk={talkStub()}
      mode="continuous"
      onModeChange={onModeChange}
      onRoute={vi.fn()}
      onOpenVoiceSettings={onOpenVoiceSettings}
      {...props}
    />,
  );
  fireEvent.click(screen.getByRole('button', { name: /voice options/i }));
  return { onModeChange, onOpenVoiceSettings };
}

describe('the composer’s voice options', () => {
  it('offers the device pickers the composer never had', () => {
    open();
    // The selector owns its own labels; what matters here is that it is
    // reachable from chat at all, which it was not before.
    expect(screen.getByRole('combobox', { name: /microphone/i })).toBeTruthy();
    expect(screen.getByRole('combobox', { name: /speaker/i })).toBeTruthy();
  });

  it('switches between push-to-talk and continuous', () => {
    const { onModeChange } = open({ mode: 'continuous' });
    fireEvent.click(screen.getByRole('checkbox', { name: /continuous/i }));
    expect(onModeChange).toHaveBeenCalledWith('push_to_talk');
  });

  it('refuses transcription with no backend, and points at the fix', () => {
    const { onOpenVoiceSettings } = open({
      talk: talkStub({ status: { configured: false } as TalkStatus }),
    });
    expect(screen.getByRole('alert').textContent).toMatch(/no transcription backend/i);
    fireEvent.click(screen.getByRole('button', { name: /open voice settings/i }));
    expect(onOpenVoiceSettings).toHaveBeenCalled();
  });

  it('claims nothing about a backend it has not read yet', () => {
    // `status` is null until the hook is enabled. Warning before then would be
    // a guess, and a guess that tells the operator their setup is broken.
    open({ talk: talkStub({ status: null }) });
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('hides the continuous toggle for the realtime engine, which it cannot drive', () => {
    // Realtime turn detection is persisted configuration, not a runtime mode.
    // A checkbox wired to `setMode` would silently do nothing there.
    open({ engine: 'realtime' });
    expect(screen.queryByRole('checkbox', { name: /continuous/i })).toBeNull();
  });
});
