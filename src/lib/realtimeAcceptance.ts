import { invoke } from '@tauri-apps/api/core';

import { realtimeVoiceClient, type RealtimeVoiceStatus } from './companionClient';
import { OpenAiRealtimeVoiceProvider } from './openAiRealtimeVoice';
import {
  RealtimeVoiceController,
  type RealtimeVoiceEvent,
  type RealtimeVoiceProvider,
  type RealtimeVoiceSession,
} from './realtimeVoice';
import {
  appendRealtimeTranscript,
  buildRealtimeToolSurface,
  executeRealtimeToolCall,
  type RealtimeToolSurface,
} from './realtimeVoiceToolBridge';
import { useSessionStore } from '../store/sessionStore';

export const REALTIME_ACCEPTANCE_STEPS = [
  'provider_configured',
  'session_connected',
  'microphone_audio_reached_provider',
  'transcript_persisted',
  'tool_call_bridged',
  'tool_result_returned',
  'spoken_followup',
  'barge_in',
  'durable_conversation',
  'clean_disconnect',
] as const;

export type RealtimeAcceptanceStepId = typeof REALTIME_ACCEPTANCE_STEPS[number];

export interface RealtimeAcceptanceStep {
  id: RealtimeAcceptanceStepId;
  status: 'passed' | 'failed';
  /** Structural evidence only — never transcript text, file content, provider
   * payloads, or anything derived from a credential. */
  detail: string;
}

export interface RealtimeAcceptanceReport {
  status: 'passed' | 'failed';
  model: string;
  turnDetection: 'semantic_vad' | 'manual';
  steps: RealtimeAcceptanceStep[];
  /** Bounded timing/interruption counters, same shape the metrics ring keeps. */
  metrics: {
    connectionMs: number | null;
    firstRecognizedSpeechMs: number | null;
    firstAudioMs: number | null;
    endToEndMs: number | null;
    interrupted: boolean;
    toolRoundTripMs: number[];
    reconnectCount: number;
  };
  /** How many times the spoken follow-up was requested through the response
   * ledger. Exactly one continuation is expected for one tool call. */
  continuationsRequested: number;
  error: string | null;
}

export interface RealtimeAcceptanceOptions {
  /** An ordinary open chat session; the acceptance run uses no private store. */
  chatSessionId: string;
  /** A harmless readable file inside the active workspace. */
  testPath: string;
  provider?: RealtimeVoiceProvider;
  status?: () => Promise<RealtimeVoiceStatus>;
  surface?: RealtimeToolSurface;
  model?: string;
  voice?: string;
  turnDetection?: 'semantic_vad' | 'manual';
  /** How long the operator has to speak the request after capture opens. */
  speakWindowMs?: number;
  exchangeTimeoutMs?: number;
  wait?: (ms: number) => Promise<void>;
  log?: (line: string) => void;
}

const OPENAI_REALTIME_ENDPOINT = 'https://api.openai.com/v1/realtime/calls';

function defaultWait(ms: number): Promise<void> {
  return new Promise((resolve) => { setTimeout(resolve, ms); });
}

function messagesOf(chatSessionId: string) {
  return useSessionStore.getState().sessions.find((candidate) => candidate.id === chatSessionId)?.messages ?? [];
}

/**
 * The ten-step real-provider acceptance for desktop realtime voice, as one
 * ordinary function so it can be driven two ways: by the desktop-hosted
 * acceptance runner against OpenAI with a live microphone
 * (`scripts/realtime-live-acceptance.mjs`), and by the unit suite against a
 * scripted provider. Both drive the same response ledger and the same tool
 * bridge the product uses, so a regression in either fails both.
 *
 * The operator speaks one short request during the capture window. No
 * credential, transcript, audio, or file content is placed in the report.
 */
export async function runRealtimeAcceptance(
  options: RealtimeAcceptanceOptions,
): Promise<RealtimeAcceptanceReport> {
  const wait = options.wait ?? defaultWait;
  const log = options.log ?? (() => undefined);
  const model = options.model ?? 'gpt-realtime-2.1';
  const turnDetection = options.turnDetection ?? 'manual';
  const controller = new RealtimeVoiceController();
  const steps = new Map<RealtimeAcceptanceStepId, RealtimeAcceptanceStep>();
  const pass = (id: RealtimeAcceptanceStepId, detail: string) => {
    if (!steps.has(id)) log(`✓ ${id}`);
    steps.set(id, { id, status: 'passed', detail });
  };
  const fail = (id: RealtimeAcceptanceStepId, detail: string) => {
    steps.set(id, { id, status: 'failed', detail });
  };

  const voiceTurnId = `vt_acceptance_${crypto.randomUUID()}`;
  const realtimeSessionId = `rv_acceptance_${crypto.randomUUID()}`;
  let session: RealtimeVoiceSession | null = null;
  let continuationsRequested = 0;
  let error: string | null = null;

  // Every step below is failed up front, so a run that dies early reports
  // which step it never reached instead of reporting nothing.
  for (const id of REALTIME_ACCEPTANCE_STEPS) fail(id, 'not reached');

  try {
    const status = await (options.status ?? realtimeVoiceClient.status)();
    if (!status.configured) throw new Error('No OpenAI key is available through the native keychain boundary.');
    if (status.endpoint !== OPENAI_REALTIME_ENDPOINT) {
      throw new Error(`The native broker reported an unexpected signaling endpoint: ${status.endpoint}`);
    }
    pass('provider_configured', `provider=${status.providerId} endpoint=fixed`);

    const surface = options.surface ?? await buildRealtimeToolSurface([]);
    const readFile = surface.tools.find((tool) => tool.function.name === 'read_file');
    if (!readFile) throw new Error('The active workspace does not offer read_file; open a workspace first.');

    let inputTranscriptLength = 0;
    let toolCallsBridged = 0;
    let toolResultsReturned = 0;
    let audioAfterToolResult = false;
    let spokenTranscriptLength = 0;
    let interrupted = false;
    let audioEventsAfterInterrupt = 0;
    let bargeInRequested = false;
    let settled!: () => void;
    const finished = new Promise<void>((resolve) => { settled = resolve; });
    const maybeFinish = () => {
      if (inputTranscriptLength > 0 && toolResultsReturned > 0 && audioAfterToolResult
        && spokenTranscriptLength > 0 && interrupted) settled();
    };
    let tail: Promise<void> = Promise.resolve();

    const onEvent = (event: RealtimeVoiceEvent) => {
      if (!controller.consume(event)) return;
      const current = session;
      if (event.type === 'input_transcript') {
        inputTranscriptLength = event.text.trim().length;
        if (inputTranscriptLength > 0) {
          pass('microphone_audio_reached_provider', `provider transcribed ${inputTranscriptLength} characters of live microphone audio`);
          const stored = appendRealtimeTranscript(
            options.chatSessionId, realtimeSessionId, event.itemId, event.eventId, 'user', event.text, voiceTurnId,
          );
          if (stored) pass('transcript_persisted', 'the spoken request entered the ordinary chat session');
        }
        maybeFinish();
        return;
      }
      if (event.type === 'output_transcript_done') {
        if (toolResultsReturned > 0) {
          spokenTranscriptLength = event.text.trim().length;
          appendRealtimeTranscript(
            options.chatSessionId, realtimeSessionId, event.itemId, event.eventId, 'assistant', event.text, voiceTurnId,
          );
        }
        maybeFinish();
        return;
      }
      if (event.type === 'output_audio_started') {
        if (interrupted) audioEventsAfterInterrupt += 1;
        if (toolResultsReturned > 0 && !interrupted) {
          audioAfterToolResult = true;
          pass('spoken_followup', `the provider spoke again after the host tool result (${continuationsRequested} continuation requested)`);
          if (!bargeInRequested) {
            bargeInRequested = true;
            void current?.interrupt();
          }
        }
        maybeFinish();
        return;
      }
      if (event.type === 'interrupted') {
        interrupted = true;
        maybeFinish();
        return;
      }
      if (event.type === 'tool_call') {
        if (event.call.name !== 'read_file') {
          fail('tool_call_bridged', `the provider asked for ${event.call.name}; the acceptance run expects read_file`);
          return;
        }
        toolCallsBridged += 1;
        pass('tool_call_bridged', 'the provider function call entered the ordinary tool executor');
        tail = tail.catch(() => undefined).then(async () => {
          const result = await executeRealtimeToolCall(event.call, {
            chatSessionId: options.chatSessionId,
            realtimeSessionId,
            voiceTurnId,
            surface,
          });
          const failed = (() => {
            try { return Boolean((JSON.parse(result) as { error?: unknown }).error); } catch { return false; }
          })();
          if (failed) fail('tool_result_returned', 'the host tool returned an error result');
          else {
            toolResultsReturned += 1;
            pass('tool_result_returned', `the host result (${result.length} bytes) was handed back to the provider`);
          }
          session?.sendToolResult(event.call.id, result);
          if (controller.settleToolCall(event.call.id) === 'continue') {
            continuationsRequested += 1;
            session?.requestResponse();
          }
          maybeFinish();
        });
        return;
      }
      if (event.type === 'response_done') {
        const disposition = controller.responseDisposition(event.responseId);
        if (disposition === 'continue') {
          continuationsRequested += 1;
          current?.requestResponse();
        }
        return;
      }
      if (event.type === 'error' || event.type === 'connection_lost') {
        error ??= event.type === 'error' ? event.message : `connection lost: ${event.code}`;
        settled();
      }
    };

    controller.connecting();
    session = (options.provider ?? new OpenAiRealtimeVoiceProvider()).createSession({
      sessionId: realtimeSessionId,
      model,
      voice: options.voice ?? 'marin',
      turnDetection,
      inputDeviceId: null,
      outputDeviceId: null,
      instructions: [
        'This is a Little Monkey acceptance run.',
        `After the user speaks, call read_file exactly once with path ${JSON.stringify(options.testPath)}.`,
        'When its result arrives, speak one short sentence summarizing the file. Do not call any other tool.',
      ].join(' '),
      tools: [readFile],
    }, onEvent);
    await session.connect();
    if (session.state !== 'ready') throw new Error(`The session did not become ready: ${session.state}`);
    pass('session_connected', `WebRTC peer and data channel are open for ${model}`);

    log(`Speak now — ask Little Monkey to read ${options.testPath}.`);
    if (turnDetection === 'manual') await session.startManualTurn();
    await wait(options.speakWindowMs ?? 8_000);
    if (turnDetection === 'manual') await session.finishManualTurn();

    await Promise.race([
      finished,
      wait(options.exchangeTimeoutMs ?? 90_000).then(() => {
        throw new Error('Timed out waiting for the spoken exchange, host tool result, and follow-up answer.');
      }),
    ]);
    await tail.catch(() => undefined);
    if (error) throw new Error(error);
    if (toolCallsBridged === 0) throw new Error('The provider never requested the host tool.');
    if (audioEventsAfterInterrupt > 0) {
      fail('barge_in', `${audioEventsAfterInterrupt} further output-audio starts arrived after the interruption`);
    } else if (interrupted) {
      pass('barge_in', 'the spoken answer stopped on interruption and no further output audio began');
    }

    const realtime = messagesOf(options.chatSessionId).filter((message) => message.realtime?.voiceTurnId === voiceTurnId);
    const kinds = new Set(realtime.map((message) => message.realtime?.kind));
    const complete = ['input_transcript', 'tool_call', 'tool_result', 'output_transcript']
      .filter((kind) => !kinds.has(kind as never));
    if (complete.length > 0) fail('durable_conversation', `missing durable rows: ${complete.join(', ')}`);
    else pass('durable_conversation', `${realtime.length} rows for one voice turn in the ordinary chat session`);

    await session.close();
    session = null;
    pass('clean_disconnect', 'peer, data channel, microphone tracks, and the native broker were released');
  } catch (reason) {
    error = reason instanceof Error ? reason.message : String(reason);
  } finally {
    await session?.close().catch(() => undefined);
  }

  const ordered = REALTIME_ACCEPTANCE_STEPS.map((id) => steps.get(id)!);
  return {
    status: error === null && ordered.every((step) => step.status === 'passed') ? 'passed' : 'failed',
    model,
    turnDetection,
    steps: ordered,
    metrics: {
      connectionMs: controller.metrics.connectionMs,
      firstRecognizedSpeechMs: controller.metrics.firstRecognizedSpeechMs,
      firstAudioMs: controller.metrics.firstAudioMs,
      endToEndMs: controller.metrics.endToEndMs,
      interrupted: controller.metrics.interrupted,
      toolRoundTripMs: controller.metrics.toolRoundTripMs,
      reconnectCount: controller.metrics.reconnectCount,
    },
    continuationsRequested,
    error,
  };
}

/**
 * Boot hook for the desktop-hosted acceptance runner
 * (`pnpm test:realtime:live`). It runs inside the real webview, so WebRTC, the
 * microphone, the native keychain broker, the session store, and the tool
 * executor are all the production ones. The native side writes the report and
 * exits the app, so the runner cannot pass on a webview that stalled.
 */
export async function reportRealtimeAcceptanceFromEnvironment(): Promise<void> {
  const testPath = String(import.meta.env.VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH ?? '');
  let report: RealtimeAcceptanceReport;
  try {
    if (!testPath) {
      throw new Error('VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH must name a harmless readable file in the workspace.');
    }
    const store = useSessionStore.getState();
    const chatSessionId = store.activeSessionId;
    if (!store.sessions.some((candidate) => candidate.id === chatSessionId)) {
      throw new Error('No ordinary chat session is open.');
    }
    report = await runRealtimeAcceptance({
      chatSessionId,
      testPath,
      log: (line) => console.log(`[realtime-acceptance] ${line}`),
    });
  } catch (reason) {
    report = {
      status: 'failed',
      model: 'gpt-realtime-2.1',
      turnDetection: 'manual',
      steps: REALTIME_ACCEPTANCE_STEPS.map((id) => ({ id, status: 'failed' as const, detail: 'not reached' })),
      metrics: {
        connectionMs: null, firstRecognizedSpeechMs: null, firstAudioMs: null,
        endToEndMs: null, interrupted: false, toolRoundTripMs: [], reconnectCount: 0,
      },
      continuationsRequested: 0,
      error: reason instanceof Error ? reason.message : String(reason),
    };
  }
  await invoke('realtime_voice_acceptance_report', { report }).catch((reason) => {
    console.error('[realtime-acceptance] could not write evidence', reason);
  });
}
