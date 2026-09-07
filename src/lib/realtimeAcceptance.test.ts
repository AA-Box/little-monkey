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
  RealtimeAudioProgress,
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
/** Rides along on every audio-progress reading the fake transport hands back.
 * Counters describing the audio are legitimate evidence; anything lifted out
 * of the audio itself is not, and a report built by stringifying a progress
 * object rather than naming its counters would carry this marker. */
const AUDIO_FINGERPRINT = 'AUDIO_WAVEFORM_ECHO_7b31 sk-live-audio-not-a-real-key';

const OPENAI_ENDPOINT = 'https://api.openai.com/v1/realtime/calls';
// Three distinct durations, because the injected clock below decides what to
// do with a window by the number it is asked to wait for.
const SPEAK_WINDOW_MS = 5_000;
const EXCHANGE_TIMEOUT_MS = 250;
const SILENCE_WINDOW_MS = 30_000;

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

/**
 * A controllable stand-in for the RTCRtpReceiver statistics the real adapter
 * reads. `advance()` is the only thing that moves the numbers, so a test —
 * not the transport — decides whether the speaker is still producing sound
 * across the silence window.
 *
 * Every read hands back a fresh snapshot on purpose. Sharing one mutable
 * object would make the harness compare a reading with itself, and then a
 * transport that never stops playing would sail through the barge-in step.
 */
function audioMeter() {
  let ticks = 0;
  const read = (): RealtimeAudioProgress & { audioFingerprint: string } => ({
    bytesReceived: 4_800 * ticks,
    samplesReceived: 24_000 * ticks,
    audioEnergy: 0.25 * ticks,
    playbackSeconds: 0.5 * ticks,
    measured: true,
    audioFingerprint: AUDIO_FINGERPRINT,
  });
  return {
    read,
    advance() {
      ticks += 1;
      return read();
    },
  };
}

interface ScriptOptions {
  /** The provider produces no follow-up response at all after the tool result
   * — the original Blocker-1 symptom this harness exists to catch. */
  silentAfterToolResult?: boolean;
  /** The provider generates a transcript after the tool result but no audio
   * ever plays out: generation without sound, the reviewer's first false pass. */
  generatesWithoutPlaying?: boolean;
  /** `pc.ontrack` never fires, so no audio could have been heard however much
   * the data channel had to say. */
  noRemoteTrack?: boolean;
  /** Further measured-playback events arriving after the interruption. */
  audioEventsAfterInterrupt?: number;
  /** What the transport can say about played-out audio: `measured` reads the
   * meter, `unmeasurable` answers null, `unimplemented` offers no method, and
   * `unreported` answers with a reading whose zeros mean "this webview does not
   * report audio energy or sample counts" rather than "the speaker was silent". */
  playbackMeasurement?: 'measured' | 'unmeasurable' | 'unimplemented' | 'unreported';
  /** A transport error arriving between the tool result and the spoken answer. */
  errorAfterToolResult?: string;
  toolName?: string;
}

/** Distributes `Omit` across the event union so a scripted event can be
 * written without the id the transport assigns. */
type ScriptedEvent<T> = T extends { eventId: string } ? Omit<T, 'eventId'> : never;

/**
 * A provider that speaks the OpenAI realtime event shape and nothing else: the
 * remote media track arriving at negotiation time the way `pc.ontrack` does,
 * one tool call attributed to its response, `response.done` arriving *before*
 * the host tool finishes (the race that Blocker 1 lived in), the follow-up
 * response only ever emitted from `requestResponse()`, and generation events
 * kept strictly separate from measured playback. The acceptance run cannot
 * tell it apart from the real transport, so a regression in the ledger or the
 * bridge fails here exactly as it would against OpenAI.
 */
function scriptedProvider(script: ScriptOptions = {}) {
  const configs: RealtimeVoiceSessionConfig[] = [];
  const toolResults: Array<{ callId: string; output: string }> = [];
  const meter = audioMeter();
  const measurement = script.playbackMeasurement ?? 'measured';
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
          if (!script.noRemoteTrack) emit({ type: 'remote_audio_track' });
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
          // Generation and text land first, exactly as they do over the data
          // channel. Neither of them is audio.
          emit({ type: 'output_generation_started' });
          emit({ type: 'output_transcript_delta', itemId: 'item_answer', delta: SPOKEN_ANSWER.slice(0, 12) });
          emit({ type: 'output_transcript_done', itemId: 'item_answer', text: SPOKEN_ANSWER });
          if (!script.generatesWithoutPlaying) {
            emit({ type: 'output_audio_playing', progress: meter.advance() });
          }
          emit({ type: 'response_done', responseId: 'resp_2', status: 'cancelled' });
        },
        async interrupt() {
          state = 'listening';
          emit({ type: 'interrupted', itemId: 'item_answer' });
          for (let extra = script.audioEventsAfterInterrupt ?? 0; extra > 0; extra -= 1) {
            emit({ type: 'output_audio_playing', progress: meter.advance() });
          }
        },
        async close() {
          closes += 1;
          state = 'closed';
        },
        ...(measurement === 'unimplemented'
          ? {}
          : {
            audioProgress: async () => {
              if (measurement === 'unmeasurable') return null;
              // 'unreported' is the WebKit shape: getStats answers, but with
              // neither `totalAudioEnergy` nor `totalSamplesReceived`, so the
              // zeros it carries mean "not told" rather than "silent".
              if (measurement === 'unreported') return { ...meter.read(), measured: false };
              return meter.read();
            },
          }),
      };
      return session;
    },
  };
  return { provider, configs, toolResults, meter, closes: () => closes };
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
  /** Keeps the meter climbing while the run observes silence, which is what a
   * speaker that ignored the interruption looks like from the outside. */
  keepsPlayingAcrossWindow?: boolean;
} = {}) {
  const fake = scriptedProvider(options.script);
  let deadlineReached!: () => void;
  const deadline = new Promise<void>((resolve) => { deadlineReached = resolve; });
  const waits: number[] = [];
  const startedAt = Date.now();
  const pending = runRealtimeAcceptance({
    chatSessionId: 'chat',
    testPath: 'CHANGELOG.md',
    provider: fake.provider,
    status: brokerStatus(options.status ?? {}),
    surface: options.surface ?? surface,
    speakWindowMs: SPEAK_WINDOW_MS,
    exchangeTimeoutMs: EXCHANGE_TIMEOUT_MS,
    silenceWindowMs: SILENCE_WINDOW_MS,
    // The run's whole sense of time. The capture and silence windows are
    // stepped over at once — thirty-five seconds of them — while the exchange
    // deadline is held open until the scripted exchange has had its chance, so
    // a healthy run never races the clock and a silent provider still hits a
    // real timeout. Whatever the speaker is doing during the silence window
    // happens here, because from the run's point of view that window *is* the
    // wait.
    wait: async (ms) => {
      waits.push(ms);
      if (ms === EXCHANGE_TIMEOUT_MS) return deadline;
      if (ms === SILENCE_WINDOW_MS && options.keepsPlayingAcrossWindow) fake.meter.advance();
      return undefined;
    },
  });
  await Promise.race([pending.catch(() => undefined), drain()]);
  deadlineReached();
  return { report: await pending, fake, waits, elapsedMs: () => Date.now() - startedAt };
}

function step(report: RealtimeAcceptanceReport, id: RealtimeAcceptanceStepId) {
  return report.steps.find((candidate) => candidate.id === id)!;
}

function firstFailure(report: RealtimeAcceptanceReport): RealtimeAcceptanceStepId | null {
  return report.steps.find((candidate) => candidate.status === 'failed')?.id ?? null;
}

function failedSteps(report: RealtimeAcceptanceReport): RealtimeAcceptanceStepId[] {
  return report.steps.filter((candidate) => candidate.status === 'failed').map((candidate) => candidate.id);
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
  it('passes all eleven steps, in order, for one tool call and one continuation', async () => {
    const { report, fake } = await run();
    expect(report.error).toBeNull();
    expect(report.status).toBe('passed');
    expect(REALTIME_ACCEPTANCE_STEPS).toHaveLength(11);
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
    // Separate evidence for separate claims: the data channel said generation
    // began, and the receiver statistics said sound came out.
    expect(step(report, 'spoken_followup').detail)
      .toBe('the provider generated a further answer after the host tool result (1 continuation requested)');
    expect(step(report, 'audio_reached_the_speaker').detail)
      .toBe('receiver statistics advanced while the answer played: 4800 bytes, energy 2.50e-1');
    // firstAudioMs is anchored on measured playback, so it exists only because
    // audio actually played.
    expect(report.metrics.firstAudioMs).toBeGreaterThanOrEqual(0);
  });

  it('FALSE PASS 1: a provider that generates a transcript but never plays audio fails audio_reached_the_speaker', async () => {
    const { report, fake } = await run({ script: { generatesWithoutPlaying: true } });
    expect(report.status).toBe('failed');
    // Generation after the tool result is genuinely proven — a transcript
    // delta is evidence of that and of nothing else — so the run must not
    // blame the follow-up for a silent speaker.
    expect(step(report, 'spoken_followup').status).toBe('passed');
    expect(firstFailure(report)).toBe('audio_reached_the_speaker');
    expect(step(report, 'audio_reached_the_speaker').status).toBe('failed');
    // Never anchored, because nothing was ever measured playing.
    expect(report.metrics.firstAudioMs).toBeNull();
    // The run waits for measured playback rather than settling on the
    // transcript, which is why this ends in the exchange timeout.
    expect(report.error).toMatch(/Timed out waiting for the spoken exchange/);
    expect(fake.toolResults).toHaveLength(1);
    expect(report.continuationsRequested).toBe(1);
    expect(fake.closes()).toBe(1);
  });

  it('fails audio_reached_the_speaker when no remote audio track ever arrived', async () => {
    const { report } = await run({ script: { noRemoteTrack: true } });
    expect(report.status).toBe('failed');
    // Over WebRTC the remote track is the only path audio can travel, so its
    // absence outranks every playing event the data channel could imply.
    expect(step(report, 'audio_reached_the_speaker')).toMatchObject({
      status: 'failed',
      detail: 'no remote audio track ever arrived, so nothing could be heard',
    });
    expect(failedSteps(report)).toEqual(['audio_reached_the_speaker']);
    expect(report.error).toBeNull();
  });

  it('FALSE PASS 2: audio that keeps playing across the silence window fails barge_in', async () => {
    const { report } = await run({ keepsPlayingAcrossWindow: true });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('barge_in');
    // No further playing event arrived — the old check would have called that
    // silence. The numbers that only move while sound is rendered say the
    // speaker never stopped.
    expect(step(report, 'barge_in').detail)
      .toBe('audio kept playing for 30000ms after the interruption (energy +2.50e-1)');
    expect(report.metrics.interrupted).toBe(true);
    // Everything else still held, so the report points at the one guarantee
    // that broke rather than collapsing the whole run.
    expect(failedSteps(report)).toEqual(['barge_in']);
  });

  it('passes barge_in when played-out audio stops advancing across the window, and says what it held at', async () => {
    const { report } = await run();
    expect(step(report, 'barge_in')).toMatchObject({
      status: 'passed',
      detail: 'played-out audio stopped advancing within 30000ms of the interruption '
        + '(energy held at 2.50e-1, playback at 0.50s)',
    });
  });

  it('blames the webview, not the app, when getStats reports no audio energy or sample count', async () => {
    // WebKit's getStats is narrower than Chromium's, and this ships to
    // WKWebView and WebKitGTK as well as WebView2. A reading of zero there
    // means "not reported", so failing with the ordinary silent-speaker
    // message would send the operator hunting a bug in the app.
    const { report } = await run({
      script: { playbackMeasurement: 'unreported', generatesWithoutPlaying: true },
    });
    expect(report.status).toBe('failed');
    expect(step(report, 'audio_reached_the_speaker')).toMatchObject({
      status: 'failed',
      detail: 'this webview reports no audio energy or sample count on inbound-rtp, so playback cannot be proven either way'
        + ' — re-run the acceptance on a webview whose getStats reports them',
    });
    // The generation half still passed: the model did answer, and only the
    // proof that it was audible is missing.
    expect(step(report, 'spoken_followup').status).toBe('passed');
    expect(failedSteps(report)).toEqual(['audio_reached_the_speaker', 'barge_in']);
  });

  it.each(['unimplemented', 'unmeasurable', 'unreported'] as const)(
    'fails barge_in as unproven when the transport answers %s for played-out audio',
    async (playbackMeasurement) => {
      const { report } = await run({ script: { playbackMeasurement } });
      expect(report.status).toBe('failed');
      // Unmeasurable is not silent. A transport that cannot see the speaker
      // must not be able to certify that it stopped.
      expect(step(report, 'barge_in')).toMatchObject({
        status: 'failed',
        detail: 'the transport could not measure played-out audio, so the stop is unproven',
      });
      expect(failedSteps(report)).toEqual(['barge_in']);
      // The interruption itself was acknowledged; only the proof is missing.
      expect(report.metrics.interrupted).toBe(true);
    },
  );

  it('fails barge_in when further measured-playback events arrive after the interruption', async () => {
    const { report } = await run({ script: { audioEventsAfterInterrupt: 2 } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('barge_in');
    expect(step(report, 'barge_in').detail)
      .toBe('2 further output-audio starts arrived after the interruption');
    expect(failedSteps(report)).toEqual(['barge_in']);
  });

  it('BLOCKER 1 REGRESSION: a provider that never speaks after the tool result fails at spoken_followup', async () => {
    const { report, fake } = await run({ script: { silentAfterToolResult: true } });
    expect(report.status).toBe('failed');
    expect(firstFailure(report)).toBe('spoken_followup');
    expect(step(report, 'tool_result_returned').status).toBe('passed');
    // A timed-out run still says why each step failed rather than leaving a
    // wall of "not reached" for the operator to interpret.
    expect(step(report, 'spoken_followup').detail)
      .toBe('the provider never generated an answer after the host tool result');
    expect(step(report, 'audio_reached_the_speaker').detail).toBe('not reached');
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

  it('ends failed with the provider error and still closes the session', async () => {
    const { report, fake } = await run({ script: { errorAfterToolResult: 'data channel closed mid-exchange' } });
    expect(report.status).toBe('failed');
    expect(report.error).toBe('data channel closed mid-exchange');
    expect(step(report, 'tool_result_returned').status).toBe('passed');
    expect(firstFailure(report)).toBe('spoken_followup');
    expect(fake.closes()).toBe(1);
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

  it('carries no transcript text, file content, credential, or anything lifted out of the audio', async () => {
    const { report } = await run();
    const evidence = JSON.stringify(report);
    expect(evidence).not.toContain(SPOKEN_REQUEST);
    expect(evidence).not.toContain(SPOKEN_ANSWER);
    expect(evidence).not.toContain(FILE_BODY);
    expect(evidence).not.toContain('ACCEPTANCE_FILE_BODY');
    expect(evidence).not.toContain('sk-');
    expect(evidence).not.toContain('CHANGELOG.md');
    // Counters describing the audio are the evidence; the audio is not. A
    // detail assembled by stringifying a progress reading would carry the
    // marker the fake transport attaches to every one of them.
    expect(evidence).toContain('4800 bytes');
    expect(evidence).not.toContain('AUDIO_WAVEFORM_ECHO');
    expect(evidence).not.toContain('audioFingerprint');
    // The words really were in play — otherwise this test proves nothing.
    expect(realtimeRows().map((message) => message.content)).toContain(SPOKEN_REQUEST);
    expect(realtimeRows().map((message) => message.content)).toContain(FILE_RESULT);
  });

  it('runs entirely on injected time, so the silence window costs no real seconds', async () => {
    const { report, waits, elapsedMs } = await run();
    expect(report.status).toBe('passed');
    // Capture window, exchange deadline, silence window: the run owns no clock
    // of its own beyond these.
    expect(waits).toEqual([SPEAK_WINDOW_MS, EXCHANGE_TIMEOUT_MS, SILENCE_WINDOW_MS]);
    expect(waits.reduce((total, ms) => total + ms, 0)).toBeGreaterThan(30_000);
    expect(elapsedMs()).toBeLessThan(1_000);
  });
});
