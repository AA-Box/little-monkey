import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { RealtimeVoiceStatus } from './companionClient';
import type { ToolDef } from './llamaClient';
import {
  REALTIME_ACCEPTANCE_STEPS,
  runRealtimeAcceptance,
  type RealtimeAcceptanceReport,
  type RealtimeAcceptanceStepId,
} from './realtimeAcceptance';
import type {
  RealtimeVoiceCapabilities,
  RealtimeVoiceEvent,
  RealtimeVoiceProvider,
  RealtimeVoiceSession,
  RealtimeVoiceSessionConfig,
  RealtimeVoiceState,
} from './realtimeVoice';
import type { RealtimeToolSurface } from './realtimeVoiceToolBridge';
import { useSessionStore } from '../store/sessionStore';

const mocks = vi.hoisted(() => ({ executeToolCall: vi.fn() }));

// Only the executor is replaced: `isBlockedInPlanMode` and everything else the
// tool bridge reaches for stays real, so this suite exercises the same dedupe
// and refusal logic the product runs.
vi.mock('./turnEngine', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./turnEngine')>()),
  executeToolCall: mocks.executeToolCall,
}));

/** Distinctive strings the report must never carry: what the operator said,
 * what the model said back, and what the file held — including something that
 * looks like a key, because a read of the wrong file is exactly how one would
 * end up in an evidence artifact. */
const SPOKEN_REQUEST = 'please read the changelog file out loud for me';
const SPOKEN_ANSWER = 'The changelog lists three entries, the newest is the voice engine.';
const FILE_BODY = 'ACCEPTANCE_FILE_BODY_9f2c OPENAI_API_KEY=sk-live-acceptance-not-a-real-key';
const FILE_RESULT = JSON.stringify({ path: 'CHANGELOG.md', bytes: FILE_BODY.length, content: FILE_BODY });

const OPENAI_ENDPOINT = 'https://api.openai.com/v1/realtime/calls';
const EXCHANGE_TIMEOUT_MS = 250;

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

interface ScriptOptions {
  /** The provider never produces the follow-up response after the tool result
   * — the Blocker-1 symptom this harness exists to catch. */
  silentAfterToolResult?: boolean;
  /** Output audio that keeps starting after the interruption was honoured. */
  audioStartsAfterInterrupt?: number;
  /** A transport error arriving between the tool result and the spoken answer. */
  errorAfterToolResult?: string;
  toolName?: string;
}

/** Distributes `Omit` across the event union so a scripted event can be
 * written without the id the transport assigns. */
type ScriptedEvent<T> = T extends { eventId: string } ? Omit<T, 'eventId'> : never;

/**
 * A provider that speaks the OpenAI realtime event shape and nothing else: one
 * tool call attributed to its response, `response.done` arriving *before* the
 * host tool finishes (the race that Blocker 1 lived in), and the follow-up
 * response only ever emitted from `requestResponse()`. The acceptance run
 * cannot tell it apart from the real transport, so a regression in the ledger
 * or the bridge fails here exactly as it would against OpenAI.
 */
function scriptedProvider(script: ScriptOptions = {}) {
  const configs: RealtimeVoiceSessionConfig[] = [];
  const toolResults: Array<{ callId: string; output: string }> = [];
  let closes = 0;
  let seq = 0;

  const provider: RealtimeVoiceProvider = {
    id: 'openai',
    capabilities: CAPABILITIES,
    createSession(config, onEvent) {
      configs.push(config);
      let state: RealtimeVoiceState = 'idle';
      // Emitted synchronously so every ordering assertion below is decided by
      // the event sequence rather than by how many microtasks a test drains.
      const emit = (event: ScriptedEvent<RealtimeVoiceEvent>): void => {
        seq += 1;
        onEvent({ ...event, eventId: `ev_${seq}` } as RealtimeVoiceEvent);
      };
      const session: RealtimeVoiceSession = {
        capabilities: CAPABILITIES,
        get state() { return state; },
        async connect() {
          state = 'ready';
          emit({ type: 'connected' });
        },
        async startManualTurn() {
          state = 'listening';
          emit({ type: 'listening' });
          emit({ type: 'speech_started', itemId: 'item_user' });
        },
        async finishManualTurn() {
          emit({ type: 'input_transcript', itemId: 'item_user', text: SPOKEN_REQUEST });
          emit({ type: 'response_started', responseId: 'resp_1' });
          emit({
            type: 'tool_call',
            responseId: 'resp_1',
            call: {
              id: 'call_1',
              itemId: 'item_call_1',
              name: script.toolName ?? 'read_file',
              arguments: JSON.stringify({ path: 'CHANGELOG.md' }),
            },
          });
          emit({
            type: 'response_done',
            responseId: 'resp_1',
            status: 'completed',
            usage: { inputTokens: 421, outputTokens: 37 },
          });
        },
        sendToolResult(callId, output) {
          toolResults.push({ callId, output });
        },
        requestResponse() {
          if (script.silentAfterToolResult) return;
          if (script.errorAfterToolResult !== undefined) {
            emit({ type: 'error', code: 'data_channel_closed', message: script.errorAfterToolResult });
            return;
          }
          emit({ type: 'response_started', responseId: 'resp_2' });
          emit({ type: 'output_transcript_delta', itemId: 'item_answer', delta: SPOKEN_ANSWER.slice(0, 12) });
          emit({ type: 'output_transcript_done', itemId: 'item_answer', text: SPOKEN_ANSWER });
          emit({ type: 'output_audio_started' });
          emit({ type: 'response_done', responseId: 'resp_2', status: 'cancelled' });
        },
        async interrupt() {
          state = 'listening';
          emit({ type: 'interrupted', itemId: 'item_answer' });
          for (let extra = script.audioStartsAfterInterrupt ?? 0; extra > 0; extra -= 1) {
            emit({ type: 'output_audio_started' });
          }
        },
        async close() {
          closes += 1;
          state = 'closed';
        },
      };
      return session;
    },
  };
  return { provider, configs, toolResults, closes: () => closes };
}

function brokerStatus(overrides: Partial<RealtimeVoiceStatus>): () => Promise<RealtimeVoiceStatus> {
  return async () => ({
    providerId: 'openai',
    configured: true,
    activeSessions: 0,
    endpoint: OPENAI_ENDPOINT,
    ...overrides,
  });
}

/** Lets the scripted exchange and the SHA-256 call key settle without leaning
 * on a timer the acceptance run itself owns. */
async function drain(): Promise<void> {
  for (let tick = 0; tick < 10; tick += 1) {
    await new Promise((resolve) => { setTimeout(resolve, 0); });
  }
}

async function run(options: {
  script?: ScriptOptions;
  status?: Partial<RealtimeVoiceStatus>;
  surface?: RealtimeToolSurface;
} = {}) {
  const fake = scriptedProvider(options.script);
  let deadlineReached!: () => void;
  const deadline = new Promise<void>((resolve) => { deadlineReached = resolve; });
  const pending = runRealtimeAcceptance({
    chatSessionId: 'chat',
    testPath: 'CHANGELOG.md',
    provider: fake.provider,
    status: brokerStatus(options.status ?? {}),
    surface: options.surface ?? surface,
    speakWindowMs: 1,
    exchangeTimeoutMs: EXCHANGE_TIMEOUT_MS,
    // The capture window elapses at once; the exchange deadline is held open
    // until the scripted exchange has had its chance, so a healthy run never
    // races the clock and a silent provider still hits a real timeout.
    wait: async (ms) => (ms === EXCHANGE_TIMEOUT_MS ? deadline : undefined),
  });
  await Promise.race([pending.catch(() => undefined), drain()]);
  deadlineReached();
  return { report: await pending, fake };
}

function step(report: RealtimeAcceptanceReport, id: RealtimeAcceptanceStepId) {
  return report.steps.find((candidate) => candidate.id === id)!;
}

function firstFailure(report: RealtimeAcceptanceReport): RealtimeAcceptanceStepId | null {
  return report.steps.find((candidate) => candidate.status === 'failed')?.id ?? null;
}

function realtimeRows() {
  return useSessionStore.getState().sessions[0].messages.filter((message) => message.realtime !== undefined);
}

beforeEach(() => {
  mocks.executeToolCall.mockReset();
  mocks.executeToolCall.mockResolvedValue(FILE_RESULT);
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

describe('realtime acceptance run against a scripted provider', () => {
  it('passes all ten steps, in order, for one tool call and one continuation', async () => {
    const { report, fake } = await run();
    expect(report.error).toBeNull();
    expect(report.status).toBe('passed');
    expect(report.steps.map((entry) => entry.id)).toEqual([...REALTIME_ACCEPTANCE_STEPS]);
    expect(report.steps.filter((entry) => entry.status !== 'passed')).toEqual([]);
    // The ask for the spoken follow-up happens exactly once whichever of
    // `response.done` and the last tool result arrives second.
    expect(report.continuationsRequested).toBe(1);
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    expect(fake.toolResults).toEqual([{ callId: 'call_1', output: FILE_RESULT }]);
    expect(fake.closes()).toBe(1);
    // Only the one tool the run needs is offered to the provider.
    expect(fake.configs[0].tools.map((tool) => tool.function.name)).toEqual(['read_file']);
    expect(report.metrics.interrupted).toBe(true);
  });

  it('BLOCKER 1 REGRESSION: a provider that never speaks after the tool result fails at spoken_followup', async () => {
    const { report, fake } = await run({ script: { silentAfterToolResult: true } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('spoken_followup');
    expect(step(report, 'tool_result_returned').status).toBe('passed');
    expect(step(report, 'spoken_followup').detail).toBe('not reached');
    // The failure is squarely the provider's silence: the host closed the
    // function call and the ledger did ask for the follow-up.
    expect(fake.toolResults).toHaveLength(1);
    expect(report.continuationsRequested).toBe(1);
    expect(report.error).toMatch(/Timed out waiting for the spoken exchange/);
    expect(fake.closes()).toBe(1);
  });

  it('fails at provider_configured and opens no session when no key is available', async () => {
    const { report, fake } = await run({ status: { configured: false } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('provider_configured');
    expect(report.error).toMatch(/native keychain boundary/);
    expect(fake.configs).toEqual([]);
    expect(fake.closes()).toBe(0);
  });

  it('fails at provider_configured when the broker reports a non-OpenAI signaling endpoint', async () => {
    const rogue = 'https://realtime.example.invalid/v1/realtime/calls';
    const { report, fake } = await run({ status: { endpoint: rogue } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('provider_configured');
    expect(report.error).toContain(rogue);
    // Fixed egress is a precondition, not an observation: nothing may be
    // negotiated with an endpoint the host did not expect.
    expect(fake.configs).toEqual([]);
  });

  it('refuses to start without read_file in the active workspace', async () => {
    const empty: RealtimeToolSurface = { ...surface, tools: [toolDef('write_file')] };
    const { report, fake } = await run({ surface: empty });
    expect(report.status).toBe('failed');
    expect(step(report, 'provider_configured').status).toBe('passed');
    expect(firstFailure(report)).toBe('session_connected');
    expect(report.error).toMatch(/does not offer read_file/);
    expect(fake.configs).toEqual([]);
  });

  it('fails tool_result_returned when the host tool answers with an error object', async () => {
    mocks.executeToolCall.mockResolvedValue('{"error":"File not found"}');
    const { report } = await run();
    expect(report.status).toBe('failed');
    expect(step(report, 'tool_call_bridged').status).toBe('passed');
    expect(step(report, 'tool_result_returned')).toMatchObject({
      status: 'failed',
      detail: 'the host tool returned an error result',
    });
  });

  it('fails barge_in when output audio keeps starting after the interruption', async () => {
    const { report } = await run({ script: { audioStartsAfterInterrupt: 2 } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('barge_in');
    expect(step(report, 'barge_in').detail).toBe('2 further output-audio starts arrived after the interruption');
    // Everything else still held, so the report points at the one guarantee
    // that broke rather than collapsing the whole run.
    expect(report.steps.filter((entry) => entry.status === 'failed').map((entry) => entry.id)).toEqual(['barge_in']);
  });

  it('carries no transcript text, file content, or credential into the report', async () => {
    const { report } = await run();
    const evidence = JSON.stringify(report);
    expect(evidence).not.toContain(SPOKEN_REQUEST);
    expect(evidence).not.toContain(SPOKEN_ANSWER);
    expect(evidence).not.toContain(FILE_BODY);
    expect(evidence).not.toContain('ACCEPTANCE_FILE_BODY');
    expect(evidence).not.toContain('sk-');
    expect(evidence).not.toContain('CHANGELOG.md');
    // The words really were in play — otherwise this test proves nothing.
    expect(realtimeRows().map((message) => message.content)).toContain(SPOKEN_REQUEST);
    expect(realtimeRows().map((message) => message.content)).toContain(FILE_RESULT);
  });

  it('scores durable_conversation from the ordinary chat session, whose four rows share one voice turn', async () => {
    const { report } = await run();
    const rows = realtimeRows();
    expect(rows.map((message) => message.realtime?.kind)).toEqual([
      'input_transcript', 'tool_call', 'tool_result', 'output_transcript',
    ]);
    const turnIds = new Set(rows.map((message) => message.realtime?.voiceTurnId));
    expect(turnIds.size).toBe(1);
    expect([...turnIds][0]).toMatch(/^vt_acceptance_/);
    expect(step(report, 'durable_conversation')).toMatchObject({
      status: 'passed',
      detail: '4 rows for one voice turn in the ordinary chat session',
    });
  });

  it('ends failed with the provider error and still closes the session', async () => {
    const { report, fake } = await run({ script: { errorAfterToolResult: 'data channel closed mid-exchange' } });
    expect(report.status).toBe('failed');
    expect(report.error).toBe('data channel closed mid-exchange');
    expect(step(report, 'tool_result_returned').status).toBe('passed');
    expect(firstFailure(report)).toBe('spoken_followup');
    expect(fake.closes()).toBe(1);
  });
});
