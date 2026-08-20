// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { StrictMode, useRef, useState } from 'react';

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  openUrl: vi.fn(),
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
vi.mock('@tauri-apps/plugin-opener', () => ({
  openUrl: (...args: unknown[]) => mocks.openUrl(...args),
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

async function startDictation(): Promise<string> {
  const button = await screen.findByRole('button', { name: 'Start dictation' }) as HTMLButtonElement;
  fireEvent.click(button);
  await screen.findByRole('button', { name: 'Starting dictation' });
  await waitFor(() => expect(mocks.invoke.mock.calls.some(([command]) => command === 'dictation_start')).toBe(true));
  const startCall = mocks.invoke.mock.calls.find(([command]) => command === 'dictation_start');
  const sessionId = (startCall?.[1] as { sessionId: string }).sessionId;
  emit('dictation://state', { sessionId, state: 'listening' });
  await screen.findByRole('button', { name: 'Stop dictation' });
  return sessionId;
}

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.openUrl.mockReset();
  mocks.openUrl.mockResolvedValue(undefined);
  mocks.listeners.clear();
  mocks.invoke.mockImplementation((command: string, args?: { sessionId?: string }) => {
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
    if (command === 'dictation_start') return Promise.resolve({ sessionId: args?.sessionId });
    return Promise.resolve(undefined);
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('DictationButton', () => {
  it('opens macOS speech settings when native recognition is unavailable', async () => {
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'dictation_capabilities') {
        return Promise.resolve({
          supported: false,
          platform: 'macos',
          engine: 'Apple Speech',
          supportsPartialResults: true,
          supportsOnDevice: false,
          languages: [],
        });
      }
      return Promise.resolve(undefined);
    });

    render(<Harness />);
    const button = await screen.findByRole('button', { name: 'Start dictation' }) as HTMLButtonElement;
    expect(button.disabled).toBe(false);
    fireEvent.click(button);

    await waitFor(() => expect(mocks.openUrl).toHaveBeenCalledWith(
      'x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition',
    ));
  });

  it('replaces provisional text, commits the final, and restores the caret', async () => {
    render(<Harness />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(6, 6);
    const sessionId = await startDictation();

    emit('dictation://partial', { sessionId, text: 'beta wor' });
    await waitFor(() => expect(textarea.value).toBe('alpha beta wor'));
    emit('dictation://partial', { sessionId: 'dictation-old', text: 'stale' });
    expect(textarea.value).toBe('alpha beta wor');

    emit('dictation://final', { sessionId, text: 'beta world' });
    emit('dictation://state', { sessionId, state: 'idle' });
    await waitFor(() => expect((screen.getByRole('button', { name: 'Start dictation' }) as HTMLButtonElement).disabled).toBe(false));
    expect(textarea.value).toBe('alpha beta world');
    expect(textarea.selectionStart).toBe('alpha beta world'.length);
  });

  it('Escape cancels the native session and restores the original selection', async () => {
    render(<Harness initial="say this" />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(4, 8);
    const sessionId = await startDictation();

    emit('dictation://partial', { sessionId, text: 'that' });
    await waitFor(() => expect(textarea.value).toBe('say that'));
    fireEvent.keyDown(window, { key: 'Escape' });

    await waitFor(() => expect((screen.getByRole('button', { name: 'Start dictation' }) as HTMLButtonElement).disabled).toBe(false));
    expect(textarea.value).toBe('say this');
    await waitFor(() => expect(textarea.selectionStart).toBe(4));
    expect(textarea.selectionEnd).toBe(8);
    expect(mocks.invoke).toHaveBeenCalledWith('dictation_cancel', { sessionId });
  });

  it('opens microphone settings after native permission denial', async () => {
    render(<Harness />);
    const button = await screen.findByRole('button', { name: 'Start dictation' });
    fireEvent.click(button);
    await screen.findByRole('button', { name: 'Starting dictation' });
    await waitFor(() => expect(mocks.invoke.mock.calls.some(([command]) => command === 'dictation_start')).toBe(true));
    const startCall = mocks.invoke.mock.calls.find(([command]) => command === 'dictation_start');
    const sessionId = (startCall?.[1] as { sessionId: string }).sessionId;

    emit('dictation://error', {
      sessionId,
      code: 'microphone_permission_denied',
      message: 'Microphone access is disabled.',
    });

    await waitFor(() => expect(mocks.openUrl).toHaveBeenCalledWith(
      'x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone',
    ));
  });

  it('settles the final recognition before a caller sends the composer', async () => {
    const settled: string[] = [];
    render(<Harness onSettled={(value) => value && settled.push(value)} />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    const sessionId = await startDictation();

    emit('dictation://partial', { sessionId, text: 'beta wor' });
    await waitFor(() => expect((screen.getByRole('textbox') as HTMLTextAreaElement).value).toBe('alpha beta wor'));
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await Promise.resolve();
    expect(settled).toEqual([]);

    emit('dictation://final', { sessionId, text: 'beta world' });
    emit('dictation://state', { sessionId, state: 'idle' });
    await waitFor(() => expect(settled).toEqual(['alpha beta world']));
  });

  it('accepts a native listening event before dictation_start resolves', async () => {
    let resolveStart!: (value: { sessionId: string }) => void;
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
      if (command === 'dictation_start') {
        return new Promise((resolve) => {
          resolveStart = resolve as (value: { sessionId: string }) => void;
        });
      }
      return Promise.resolve(undefined);
    });

    render(<Harness />);
    fireEvent.click(await screen.findByRole('button', { name: 'Start dictation' }));
    await screen.findByRole('button', { name: 'Starting dictation' });
    await waitFor(() => expect(mocks.invoke.mock.calls.some(([command]) => command === 'dictation_start')).toBe(true));
    const startCall = mocks.invoke.mock.calls.find(([command]) => command === 'dictation_start');
    const sessionId = (startCall?.[1] as { sessionId: string }).sessionId;

    emit('dictation://state', { sessionId, state: 'listening' });
    await screen.findByRole('button', { name: 'Stop dictation' });

    resolveStart({ sessionId });
  });

  it('does not cancel a completed start under React StrictMode', async () => {
    let resolveStart!: (value: { sessionId: string }) => void;
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
      if (command === 'dictation_start') {
        return new Promise((resolve) => {
          resolveStart = resolve as (value: { sessionId: string }) => void;
        });
      }
      return Promise.resolve(undefined);
    });

    render(
      <StrictMode>
        <Harness />
      </StrictMode>,
    );
    fireEvent.click(await screen.findByRole('button', { name: 'Start dictation' }));
    await screen.findByRole('button', { name: 'Starting dictation' });
    await waitFor(() => expect(mocks.invoke.mock.calls.some(([command]) => command === 'dictation_start')).toBe(true));
    const startCall = mocks.invoke.mock.calls.find(([command]) => command === 'dictation_start');
    const sessionId = (startCall?.[1] as { sessionId: string }).sessionId;

    emit('dictation://state', { sessionId, state: 'listening' });
    await screen.findByRole('button', { name: 'Stop dictation' });
    resolveStart({ sessionId });
    await new Promise<void>((resolve) => setTimeout(resolve, 0));

    expect(mocks.invoke).not.toHaveBeenCalledWith('dictation_cancel', { sessionId });
  });

  it('waits for startup to finish before settling Send', async () => {
    let resolveStart!: (value: { sessionId: string }) => void;
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
      if (command === 'dictation_start') {
        return new Promise((resolve) => {
          resolveStart = resolve as (value: { sessionId: string }) => void;
        });
      }
      return Promise.resolve(undefined);
    });

    const settled: string[] = [];
    render(<Harness onSettled={(value) => value && settled.push(value)} />);
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    fireEvent.click(await screen.findByRole('button', { name: 'Start dictation' }));
    await screen.findByRole('button', { name: 'Starting dictation' });
    await waitFor(() => expect(mocks.invoke.mock.calls.some(([command]) => command === 'dictation_start')).toBe(true));
    const startCall = mocks.invoke.mock.calls.find(([command]) => command === 'dictation_start');
    const sessionId = (startCall?.[1] as { sessionId: string }).sessionId;

    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await Promise.resolve();
    expect(settled).toEqual([]);

    resolveStart({ sessionId });
    await waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('dictation_stop', { sessionId }));
    emit('dictation://final', { sessionId, text: 'hello' });
    emit('dictation://state', { sessionId, state: 'idle' });
    await waitFor(() => expect(settled).toEqual(['alpha hello']));
  });
});
