// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useRef, useState } from 'react';

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, Set<(event: { payload: unknown }) => void>>(),
}));

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async (name: string, handler: (event: { payload: unknown }) => void) => {
    const handlers = mocks.listeners.get(name) ?? new Set();
    handlers.add(handler);
    mocks.listeners.set(name, handlers);
    return () => handlers.delete(handler);
  },
}));

import { DictationButton, type DictationButtonHandle } from './DictationButton';

const CONFIG = {
  schemaVersion: 1,
  overlayShortcut: 'CommandOrControl+Shift+Space',
  voice: {
    backend: 'local_whisper',
    whisperBinary: null,
    whisperModel: null,
    providerId: null,
    providerModel: 'whisper-1',
    extensionId: null,
    extensionCapabilityId: null,
    language: 'auto',
    ttsVoice: null,
    ttsBackend: 'system',
    ttsExtensionId: null,
    ttsExtensionCapabilityId: null,
    realtimeBackend: 'system',
    realtimeExtensionId: null,
    realtimeExtensionCapabilityId: null,
    saveRawAudio: false,
    inputDeviceId: null,
    outputDeviceId: null,
    vadMinSpeechMs: 180,
    vadSilenceMs: 800,
    vadMaxUtteranceMs: 90_000,
    wakePhraseEnabled: false,
    wakePhrase: 'hey little monkey',
    alwaysListening: false,
    dictationLanguage: 'en-US',
    dictationRequireOnDevice: true,
  },
  imageEndpoints: [],
};

function emit(name: string, payload: unknown): void {
  for (const handler of mocks.listeners.get(name) ?? []) handler({ payload });
}

function Harness({ initial = 'alpha ', onSettled }: { initial?: string; onSettled?: (value: string | null) => void }) {
  const [value, setValue] = useState(initial);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const dictationRef = useRef<DictationButtonHandle>(null);
  return (
    <>
      <textarea ref={textareaRef} value={value} onChange={(event) => setValue(event.target.value)} />
      <DictationButton
        ref={dictationRef}
        sessionId="chat-1"
        value={value}
        onChange={setValue}
        textareaRef={textareaRef}
      />
      <button type="button" onClick={() => void dictationRef.current?.settleForSend().then(onSettled)}>
        Send
      </button>
      <output>{value}</output>
    </>
  );
}

async function startDictation(): Promise<HTMLButtonElement> {
  const button = await screen.findByRole('button', { name: 'Start dictation' }) as HTMLButtonElement;
  fireEvent.click(button);
  await screen.findByRole('button', { name: 'Starting dictation' });
  emit('dictation://state', { sessionId: 'dictation-1', state: 'listening' });
  const active = await screen.findByRole('button', { name: 'Stop dictation' }) as HTMLButtonElement;
  return active;
}

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.listeners.clear();
  mocks.invoke.mockImplementation((command: string) => {
    if (command === 'dictation_capabilities') {
      return Promise.resolve({
        supported: true,
        platform: 'macos',
        engine: 'Apple Speech',
        supportsPartialResults: true,
        supportsOnDevice: true,
        languages: [{ id: 'en-US', label: 'English (US)' }],
      });
    }
    if (command === 'm7_config_get') return Promise.resolve(CONFIG);
    if (command === 'dictation_start') return Promise.resolve({ sessionId: 'dictation-1' });
    return Promise.resolve(undefined);
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('DictationButton', () => {
  it('replaces provisional text, commits the final, and restores the caret', async () => {
    render(<Harness />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(6, 6);
    await startDictation();

    emit('dictation://partial', { sessionId: 'dictation-1', text: 'beta wor' });
    await waitFor(() => expect(textarea.value).toBe('alpha beta wor'));
    emit('dictation://partial', { sessionId: 'dictation-old', text: 'stale' });
    expect(textarea.value).toBe('alpha beta wor');

    emit('dictation://final', { sessionId: 'dictation-1', text: 'beta world' });
    emit('dictation://state', { sessionId: 'dictation-1', state: 'idle' });
    await waitFor(() => expect((screen.getByRole('button', { name: 'Start dictation' }) as HTMLButtonElement).disabled).toBe(false));
    expect(textarea.value).toBe('alpha beta world');
    expect(textarea.selectionStart).toBe('alpha beta world'.length);
  });

  it('Escape cancels the native session and restores the original selection', async () => {
    render(<Harness initial="say this" />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(4, 8);
    await startDictation();

    emit('dictation://partial', { sessionId: 'dictation-1', text: 'that' });
    await waitFor(() => expect(textarea.value).toBe('say that'));
    fireEvent.keyDown(window, { key: 'Escape' });

    await waitFor(() => expect((screen.getByRole('button', { name: 'Start dictation' }) as HTMLButtonElement).disabled).toBe(false));
    expect(textarea.value).toBe('say this');
    await waitFor(() => expect(textarea.selectionStart).toBe(4));
    expect(mocks.invoke).toHaveBeenCalledWith('dictation_cancel', { sessionId: 'dictation-1' });
  });

  it('settles the final recognition before a caller sends the composer', async () => {
    const settled: string[] = [];
    render(<Harness onSettled={(value) => value && settled.push(value)} />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    await startDictation();

    emit('dictation://partial', { sessionId: 'dictation-1', text: 'beta wor' });
    await waitFor(() => expect((screen.getByRole('textbox') as HTMLTextAreaElement).value).toBe('alpha beta wor'));
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await Promise.resolve();
    expect(settled).toEqual([]);

    emit('dictation://final', { sessionId: 'dictation-1', text: 'beta world' });
    emit('dictation://state', { sessionId: 'dictation-1', state: 'idle' });
    await waitFor(() => expect(settled).toEqual(['alpha beta world']));
  });
});
