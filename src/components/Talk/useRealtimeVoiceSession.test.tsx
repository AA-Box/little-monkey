// @vitest-environment jsdom
/**
 * The realtime voice hook driven the way a provider drives it.
 *
 * The unit tests around the controller and the tool bridge each pin one half of
 * a spoken turn; what nothing else can claim is that the two halves are wired
 * together correctly here — that the ledger the controller keeps and the
 * durable identity the bridge stamps are consulted by the same code path the
 * product runs. So this suite replaces only the provider (a fake session that
 * records every function_call_output, response request, interrupt and close)
 * and the tool executor, and lets the real hook, the real controller and the
 * real bridge decide everything else.
 *
 * The two merge blockers are the reason it exists. A tool that answered before
 * `response.done` used to leave the operator waiting in silence, and a
 * reconnect used to run the operation a second time; both are ordering bugs
 * between components, so both are only observable from here.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';

import type { VoiceConfig } from '../../lib/companionClient';
import type { ToolDef } from '../../lib/llamaClient';
import type {
  RealtimeVoiceCapabilities,
  RealtimeVoiceEvent,
  RealtimeVoiceSession,
  RealtimeVoiceSessionConfig,
  RealtimeVoiceState,
} from '../../lib/realtimeVoice';
import type { RealtimeToolSurface } from '../../lib/realtimeVoiceToolBridge';

const invoke = vi.fn(async (_command: string, _args?: unknown): Promise<unknown> => null);
const executeToolCall = vi.fn(async (..._args: unknown[]) => '{"ok":true}');

/** `isTauri` is false so the pieces that only exist inside the desktop shell —
 * the durable run ledger and session persistence — take the null path they
 * already take in a browser, leaving the hook's own logic under test. */
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (command: string, args?: unknown) => invoke(command, args),
  isTauri: () => false,
}));

/** Only the surface builder is replaced: it would otherwise enumerate MCP
 * servers and extensions over IPC. The dedupe and durable-identity logic in
 * `executeRealtimeToolCall` is the code under test, so it stays real. */
vi.mock('../../lib/realtimeVoiceToolBridge', async (importOriginal) => ({
  ...await importOriginal<typeof import('../../lib/realtimeVoiceToolBridge')>(),
  buildRealtimeToolSurface: async () => surface,
}));

/** The tool boundary itself is the one thing a test must own, because the
 * question every reconnect test asks is how many times it was crossed.
 * `isBlockedInPlanMode` stays real — it decides which unknown-outcome calls
 * the bridge refuses. */
vi.mock('../../lib/turnEngine', async (importOriginal) => ({
  ...await importOriginal<typeof import('../../lib/turnEngine')>(),
  executeToolCall: (...args: unknown[]) => executeToolCall(...args),
}));

import { useRealtimeVoiceSession, type UseRealtimeVoiceSession } from './useRealtimeVoiceSession';
import { usePermissionStore } from '../../store/permissionStore';
import { useSessionStore } from '../../store/sessionStore';

function toolDef(name: string): ToolDef {
  return {
    type: 'function',
    function: { name, description: name, parameters: { type: 'object', properties: {} } },
  };
}

const surface: RealtimeToolSurface = {
  tools: [toolDef('read_file'), toolDef('write_file')],
  mcpRegistry: new Map(),
  extensionRegistry: new Map(),
  attachedStackNames: [],
};

const CAPABILITIES: RealtimeVoiceCapabilities = {
  inputAudio: true,
  outputAudio: true,
  inputTranscription: true,
  outputTranscription: true,
  serverVad: true,
  manualTurnDetection: true,
  interruption: true,
  tools: true,
};

const sessions: FakeSession[] = [];
let connectBehaviour: (attempt: number) => Promise<void> = async () => undefined;

/** Stands in for the WebRTC session. Everything the hook can ask a provider to
 * do is recorded in `log` in order, because the blockers are about ordering:
 * a tool result must reach the provider before the follow-up is requested, and
 * the follow-up must be requested exactly once. */
class FakeSession implements RealtimeVoiceSession {
  readonly capabilities = CAPABILITIES;
  state: RealtimeVoiceState = 'idle';
  readonly log: string[] = [];
  readonly toolResults: { callId: string; output: string }[] = [];
  requests = 0;
  interrupts = 0;
  closes = 0;

  constructor(
    readonly config: RealtimeVoiceSessionConfig,
    readonly onEvent: (event: RealtimeVoiceEvent) => void,
  ) {}

  async connect(): Promise<void> {
    this.log.push('connect');
    await connectBehaviour(sessions.indexOf(this));
  }

  async interrupt(): Promise<void> {
    this.interrupts += 1;
  }

  async startManualTurn(): Promise<void> {}

  async finishManualTurn(): Promise<void> {}

  sendToolResult(callId: string, output: string): void {
    this.log.push('function_call_output');
    this.toolResults.push({ callId, output });
  }

  requestResponse(): void {
    this.log.push('response_create');
    this.requests += 1;
  }

  async close(): Promise<void> {
    this.closes += 1;
  }
}

vi.mock('../../lib/openAiRealtimeVoice', () => ({
  OpenAiRealtimeVoiceProvider: class {
    readonly id = 'openai';
    readonly capabilities = CAPABILITIES;
    createSession(
      config: RealtimeVoiceSessionConfig,
      onEvent: (event: RealtimeVoiceEvent) => void,
    ): RealtimeVoiceSession {
      const session = new FakeSession(config, onEvent);
      sessions.push(session);
      return session;
    }
  },
}));

const VOICE: VoiceConfig = {
  engineKind: 'realtime',
  realtimeProviderId: 'openai',
  realtimeModel: 'gpt-realtime-2.1',
  realtimeVoice: 'marin',
  realtimeTurnDetection: 'semantic_vad',
  backend: 'local_whisper',
  whisperBinary: null,
  whisperModel: null,
  providerId: null,
  providerModel: 'whisper-1',
  extensionId: null,
  extensionCapabilityId: null,
  language: 'auto',
  transcriptionModel: 'base',
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
  dictationLanguage: null,
  dictationRequireOnDevice: false,
};

/** jsdom has no `navigator.mediaDevices`, and the hook watches it for the
 * microphone disappearing mid-session. A real EventTarget lets a test unplug a
 * device the way the browser reports it. */
let audioInputs: { kind: string; deviceId: string; label: string }[] = [];
const mediaDevices = Object.assign(new EventTarget(), {
  enumerateDevices: async () => audioInputs,
});

type WithoutEventId<T> = T extends unknown ? Omit<T, 'eventId'> : never;
type ProviderEvent = WithoutEventId<RealtimeVoiceEvent>;

let eventSeq = 0;

/** Delivers provider events with unique ids, because the controller dedupes on
 * them and a reconnect keeps the ids it has already seen. */
function feed(session: FakeSession, ...events: ProviderEvent[]): void {
  act(() => {
    for (const event of events) {
      eventSeq += 1;
      session.onEvent({ ...event, eventId: `e${eventSeq}` } as RealtimeVoiceEvent);
    }
  });
}

function gate(): { promise: Promise<string>; release: (result: string) => void } {
  let release!: (result: string) => void;
  const promise = new Promise<string>((resolve) => { release = resolve; });
  return { promise, release };
}

type View = { result: { current: UseRealtimeVoiceSession } };

function render(voice: VoiceConfig = VOICE): View {
  return renderHook(() => useRealtimeVoiceSession('chat', voice));
}

/** A connected session, which is where every turn test starts. */
async function open(voice: VoiceConfig = VOICE): Promise<View & { session: FakeSession }> {
  const view = render(voice);
  await act(async () => { await view.result.current.start(); });
  const session = sessions[sessions.length - 1];
  feed(session, { type: 'connected' });
  return { ...view, session };
}

const messages = () => useSessionStore.getState().sessions[0].messages;
const metricsRecorded = () =>
  invoke.mock.calls.filter(([command]) => command === 'realtime_voice_metric_record').length;

beforeEach(() => {
  sessions.length = 0;
  eventSeq = 0;
  connectBehaviour = async () => undefined;
  invoke.mockClear();
  executeToolCall.mockClear();
  executeToolCall.mockImplementation(async () => '{"ok":true}');
  audioInputs = [];
  Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value: mediaDevices });
  useSessionStore.setState({
    sessions: [{
      id: 'chat', title: 'New session', messages: [], createdAt: 1, updatedAt: 1,
      pinned: false, unread: false, archived: false, groupId: null, modelTarget: null,
      comparisonBranch: null, workspacePath: null, personaId: null, attachedStackIds: [],
      docChatMode: false, subagentRuns: {}, subagentRunStats: {},
    }],
    activeSessionId: 'chat',
  } as never);
});

afterEach(() => {
  cleanup();
});

describe('a spoken turn that calls a tool', () => {
  it('BLOCKER 1 REGRESSION: speaks the follow-up when the tool answered before response.done', async () => {
    const { result, session } = await open();
    feed(
      session,
      { type: 'input_transcript', itemId: 'u1', text: 'what is in the notes file' },
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'read_file', arguments: '{"path":"notes.md"}' },
      },
    );
    // The whole tool round trip settles first, which is exactly the race the
    // old outstanding-call counter lost: it had already reached zero, so
    // `response.done` concluded there was nothing left to speak about.
    await waitFor(() => expect(session.toolResults).toHaveLength(1));
    expect(session.toolResults[0]).toEqual({ callId: 'call_1', output: '{"ok":true}' });
    expect(session.requests).toBe(0);

    feed(session, { type: 'response_done', responseId: 'r1', status: 'completed' });

    expect(session.requests).toBe(1);
    expect(session.log).toEqual(['connect', 'function_call_output', 'response_create']);
    await waitFor(() => expect(result.current.awaitingApproval).toBe(false));
  });

  it('asks for the follow-up after the result when response.done beat the slow tool', async () => {
    const slow = gate();
    executeToolCall.mockImplementation(async () => slow.promise);
    const { session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'read_file', arguments: '{"path":"notes.md"}' },
      },
    );
    await waitFor(() => expect(executeToolCall).toHaveBeenCalledTimes(1));

    feed(session, { type: 'response_done', responseId: 'r1', status: 'completed' });
    expect(session.requests).toBe(0);

    slow.release('{"lines":3}');
    await waitFor(() => expect(session.requests).toBe(1));
    // The provider is never asked to speak about a result it has not received.
    expect(session.log).toEqual(['connect', 'function_call_output', 'response_create']);
    expect(session.toolResults[0].output).toBe('{"lines":3}');
  });

  it('asks for the follow-up once when one response called two tools', async () => {
    const { session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'read_file', arguments: '{"path":"a.md"}' },
      },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_2', itemId: 'item_2', name: 'read_file', arguments: '{"path":"b.md"}' },
      },
      { type: 'response_done', responseId: 'r1', status: 'completed' },
    );
    await waitFor(() => expect(session.toolResults).toHaveLength(2));
    expect(session.requests).toBe(1);
    expect(session.log).toEqual([
      'connect', 'function_call_output', 'function_call_output', 'response_create',
    ]);
  });

  it('does not continue a response the operator interrupted while its tool ran', async () => {
    const slow = gate();
    executeToolCall.mockImplementation(async () => slow.promise);
    const { session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'write_file', arguments: '{"path":"a.md"}' },
      },
    );
    await waitFor(() => expect(executeToolCall).toHaveBeenCalledTimes(1));

    feed(session, { type: 'interrupted', itemId: 'item_1' });
    slow.release('{"ok":true}');

    // The call is still closed out — the provider conversation would be
    // malformed otherwise — but the abandoned answer is not resurrected.
    await waitFor(() => expect(session.toolResults).toHaveLength(1));
    expect(session.requests).toBe(0);
    feed(session, { type: 'response_done', responseId: 'r1', status: 'cancelled' });
    expect(session.requests).toBe(0);
  });

  it('does not continue a cancelled response whose tool already answered', async () => {
    const { session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'read_file', arguments: '{"path":"a.md"}' },
      },
    );
    await waitFor(() => expect(session.toolResults).toHaveLength(1));

    feed(session, { type: 'response_done', responseId: 'r1', status: 'cancelled' });

    expect(session.requests).toBe(0);
    expect(session.log).toEqual(['connect', 'function_call_output']);
  });
});

describe('a connection that drops', () => {
  it('BLOCKER 2 REGRESSION: a reconnect answers the reissued tool without executing it twice', async () => {
    const args = '{"path":"notes.md","content":"one"}';
    // Numbered so a second crossing of the tool boundary would be visible in
    // the output the provider receives, not only in the call count.
    let runs = 0;
    executeToolCall.mockImplementation(async () => JSON.stringify({ run: ++runs }));
    const { session } = await open();
    feed(
      session,
      { type: 'input_transcript', itemId: 'u1', text: 'append one to the notes' },
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'write_file', arguments: args },
      },
    );
    await waitFor(() => expect(session.toolResults).toHaveLength(1));

    feed(session, { type: 'connection_lost', recoverable: true, code: 'ice_failed' });
    await waitFor(() => expect(sessions).toHaveLength(2));
    const reconnected = sessions[1];
    feed(reconnected, { type: 'connected' });

    // A new provider conversation means new item and call ids for the same
    // operation. Only the host-side turn identity can tell that the file was
    // already written.
    feed(
      reconnected,
      { type: 'response_started', responseId: 'r2' },
      {
        type: 'tool_call',
        responseId: 'r2',
        call: { id: 'call_2', itemId: 'item_2', name: 'write_file', arguments: args },
      },
      { type: 'response_done', responseId: 'r2', status: 'completed' },
    );
    await waitFor(() => expect(reconnected.toolResults).toHaveLength(1));

    expect(executeToolCall).toHaveBeenCalledTimes(1);
    expect(reconnected.toolResults[0]).toEqual({ callId: 'call_2', output: '{"run":1}' });
    expect(session.closes).toBe(1);
    // Answered from the transcript, so the reissue also leaves no second pair
    // of rows for a later reader to score as two executions.
    expect(messages().map((message) => message.realtime?.kind)).toEqual([
      'input_transcript', 'tool_call', 'tool_result',
    ]);
  });

  it('reconnects once and then reports the failure instead of looping', async () => {
    const { result, session } = await open();
    feed(session, { type: 'connection_lost', recoverable: true, code: 'ice_failed' });
    await waitFor(() => expect(sessions).toHaveLength(2));
    const reconnected = sessions[1];
    feed(reconnected, { type: 'connected' });

    feed(reconnected, { type: 'connection_lost', recoverable: true, code: 'ice_failed' });

    expect(sessions).toHaveLength(2);
    expect(result.current.error).toBe(
      'The realtime connection was lost. Start again to retry the same provider.',
    );
    expect(result.current.state).toBe('error');
    expect(reconnected.closes).toBe(1);
  });

  it('reconnects through a drop while a tool is merely executing', async () => {
    // This is the exact window the durable call identity exists for: a long
    // tool with an ICE blip in the middle. Treating "a tool is running" as
    // "the operator is answering a prompt" threw the turn away instead.
    const approval = gate();
    executeToolCall.mockImplementation(async () => approval.promise);
    const { result, session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'write_file', arguments: '{"path":"a.md"}' },
      },
    );
    await waitFor(() => expect(result.current.awaitingApproval).toBe(true));

    feed(session, { type: 'connection_lost', recoverable: true, code: 'ice_failed' });

    await waitFor(() => expect(sessions).toHaveLength(2));
    approval.release('{"ok":true}');
    await waitFor(() => expect(result.current.awaitingApproval).toBe(false));
  });

  it('does not reconnect underneath a permission prompt the operator is answering', async () => {
    const approval = gate();
    executeToolCall.mockImplementation(async () => approval.promise);
    const { result, session } = await open();
    feed(
      session,
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'write_file', arguments: '{"path":"a.md"}' },
      },
    );
    await waitFor(() => expect(result.current.awaitingApproval).toBe(true));
    // The real signal is a request the operator can see, not the fact that a
    // tool is running: the modal cannot be answered against a session that has
    // been torn down underneath it.
    usePermissionStore.setState({
      pending: { id: 'req-1', tool: 'write_file', detail: 'a.md' },
    } as never);

    feed(session, { type: 'connection_lost', recoverable: true, code: 'ice_failed' });

    // Reconnecting here would re-ask a question the operator is already
    // looking at, against a session that can no longer receive the answer.
    expect(sessions).toHaveLength(1);
    expect(result.current.state).toBe('error');
    expect(result.current.error).toBe(
      'The realtime connection was lost. Start again to retry the same provider.',
    );
    approval.release('{"ok":true}');
    await waitFor(() => expect(result.current.awaitingApproval).toBe(false));
    usePermissionStore.setState({ pending: null } as never);
  });

  it('names the microphone as the reason when the operating system revokes it', async () => {
    const { result, session } = await open();
    feed(session, { type: 'connection_lost', recoverable: false, code: 'microphone_revoked' });

    expect(result.current.error).toBe(
      'Microphone access ended. Check the selected device and system permission.',
    );
    expect(result.current.state).toBe('error');
    expect(session.closes).toBe(1);
    expect(sessions).toHaveLength(1);
  });
});

describe('the session lifecycle', () => {
  it('reports a rejected credential and still starts cleanly on the retry', async () => {
    connectBehaviour = async (attempt) => {
      if (attempt === 0) throw new Error('Realtime handshake refused: 401 invalid api key');
    };
    const view = render();
    await act(async () => { await view.result.current.start(); });

    expect(view.result.current.state).toBe('error');
    expect(view.result.current.error).toContain('401 invalid api key');
    expect(sessions[0].closes).toBe(1);

    await act(async () => { await view.result.current.start(); });
    expect(sessions).toHaveLength(2);
    feed(sessions[1], { type: 'connected' });
    expect(view.result.current.state).toBe('ready');
    expect(view.result.current.error).toBeNull();
  });

  it('ends the session when the selected microphone is unplugged', async () => {
    audioInputs = [{ kind: 'audioinput', deviceId: 'mic-usb', label: 'USB Microphone' }];
    const voice: VoiceConfig = { ...VOICE, inputDeviceId: 'mic-usb' };
    const { result, session } = await open(voice);

    audioInputs = [{ kind: 'audioinput', deviceId: 'mic-builtin', label: 'MacBook Microphone' }];
    await act(async () => {
      mediaDevices.dispatchEvent(new Event('devicechange'));
      await Promise.resolve();
    });

    await waitFor(() => expect(session.closes).toBe(1));
    expect(result.current.error).toBe(
      'The selected microphone was removed. Reconnect it or choose another device.',
    );
    expect(result.current.state).toBe('error');
  });

  it('closes and records one metric when the operator stops', async () => {
    const { result, session } = await open();
    await act(async () => { await result.current.stop(); });

    expect(session.closes).toBe(1);
    expect(result.current.state).toBe('closed');
    expect(metricsRecorded()).toBe(1);

    // Stopping an already-stopped session must not file a second measurement
    // of the same call; the metric store counts sessions, not clicks.
    await act(async () => { await result.current.stop(); });
    expect(metricsRecorded()).toBe(1);
    expect(session.closes).toBe(1);
  });
});

describe('the durable transcript of a finished turn', () => {
  it('records the transcript, the call and its result under one voice turn id', async () => {
    const { session } = await open();
    feed(
      session,
      { type: 'listening' },
      { type: 'input_transcript', itemId: 'u1', text: 'what is in the notes file' },
      { type: 'response_started', responseId: 'r1' },
      {
        type: 'tool_call',
        responseId: 'r1',
        call: { id: 'call_1', itemId: 'item_1', name: 'read_file', arguments: '{"path":"notes.md"}' },
      },
    );
    await waitFor(() => expect(session.toolResults).toHaveLength(1));
    feed(
      session,
      { type: 'output_transcript_done', itemId: 'a1', text: 'It says hello.' },
      { type: 'response_done', responseId: 'r1', status: 'completed' },
    );

    await waitFor(() => expect(messages()).toHaveLength(4));
    const recorded = messages();
    expect(recorded.map((message) => message.realtime?.kind)).toEqual([
      'input_transcript', 'tool_call', 'tool_result', 'output_transcript',
    ]);
    expect(recorded.map((message) => message.role)).toEqual([
      'user', 'assistant', 'tool', 'assistant',
    ]);
    // One identity across all four rows is what lets a reconnect — or a fresh
    // process — recognize this turn's work as already done.
    const turnIds = new Set(recorded.map((message) => message.realtime?.voiceTurnId));
    expect(turnIds.size).toBe(1);
    expect([...turnIds][0]).toMatch(/^vt_/);
    expect(recorded[1].realtime?.callKey).toMatch(/^[0-9a-f]{64}$/);
    expect(recorded[2].realtime?.callKey).toBe(recorded[1].realtime?.callKey);
  });
});
