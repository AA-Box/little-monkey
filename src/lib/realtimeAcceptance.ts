import { invoke } from '@tauri-apps/api/core';

import { realtimeVoiceClient, type RealtimeVoiceStatus } from './companionClient';
import { OpenAiRealtimeVoiceProvider } from './openAiRealtimeVoice';
import {
  RealtimeVoiceController,
  type RealtimeAudioProgress,
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
  'non_silent_remote_audio_received',
  'playback_element_advancing',
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
    /** Time to *measured* playback, not to the first transcript delta. */
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
  /** How long silence is observed after the interruption before barge-in is
   * called proven. Long enough that audio still in flight would show up. */
  silenceWindowMs?: number;
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
    let generationAfterToolResult = false;
    let remoteTrackReceived = false;
    let playingAfterToolResult: RealtimeAudioProgress | null = null;
    // Held on an object because it is only ever written from inside a callback,
    // which control-flow narrowing would otherwise read as "always null".
    const playback: { beforeInterrupt: RealtimeAudioProgress | null } = { beforeInterrupt: null };
    let spokenTranscriptLength = 0;
    let interrupted = false;
    let audioEventsAfterInterrupt = 0;
    let bargeInRequested = false;
    let settled!: () => void;
    const finished = new Promise<void>((resolve) => { settled = resolve; });
    const maybeFinish = () => {
      if (inputTranscriptLength > 0 && toolResultsReturned > 0 && playingAfterToolResult !== null
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
      if (event.type === 'remote_audio_track') {
        remoteTrackReceived = true;
        return;
      }
      if (event.type === 'output_generation_started') {
        if (toolResultsReturned > 0 && !interrupted) {
          generationAfterToolResult = true;
          pass('spoken_followup', `the provider generated a further answer after the host tool result (${continuationsRequested} continuation requested)`);
        }
        return;
      }
      if (event.type === 'non_silent_remote_audio') {
        if (interrupted) audioEventsAfterInterrupt += 1;
        if (toolResultsReturned > 0 && !interrupted) {
          playingAfterToolResult = event.progress;
          pass(
            'non_silent_remote_audio_received',
            `accumulated audio energy advanced to ${event.progress.audioEnergy.toExponential(2)} over ${Math.round(event.progress.bytesReceived)} received bytes, so the answer was rendered as sound rather than silence`,
          );
          if (!bargeInRequested) {
            bargeInRequested = true;
            // Read while the answer is still playing: after `interrupt()` the
            // element is paused, and its state then says nothing about whether
            // the playback path ever worked.
            void (async () => {
              playback.beforeInterrupt = await current?.audioProgress?.() ?? event.progress;
              await current?.interrupt();
            })();
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

    // A timeout is recorded rather than thrown, so the evidence below is still
    // gathered and the report says which steps were proven before the run ran
    // out of patience. Thrown here, every remaining step would read
    // "not reached" and the operator would learn nothing about why.
    let timedOut = false;
    await Promise.race([
      finished,
      wait(options.exchangeTimeoutMs ?? 90_000).then(() => { timedOut = true; }),
    ]);
    await tail.catch(() => undefined);
    if (error) throw new Error(error);
    const meter = await session.audioProgress?.() ?? null;
    if (!remoteTrackReceived) {
      fail('non_silent_remote_audio_received', 'no remote audio track ever arrived, so nothing could be heard');
    } else if (playingAfterToolResult === null && meter !== null && !meter.audioEnergyReported) {
      // Not the app's failure: this webview does not report totalAudioEnergy,
      // so nothing here can tell sound from a silent stream.
      fail(
        'non_silent_remote_audio_received',
        'this webview reports no totalAudioEnergy on inbound-rtp, so sound cannot be told from silence either way — re-run the acceptance on a webview whose getStats reports it',
      );
    } else if (!generationAfterToolResult) {
      fail('spoken_followup', 'the provider never generated an answer after the host tool result');
    }

    // Two different claims, measured at two different layers. Receiver
    // statistics say the answer was sound; the element says the application's
    // own playback path carried it. Neither can see the output device, the OS
    // mixer, or the speaker, so neither is reported as proof a person heard it.
    const playing = playback.beforeInterrupt;
    if (playing === null) {
      fail('playback_element_advancing', 'the transport could not report the local playback element');
    } else if (!playing.playbackStarted) {
      fail('playback_element_advancing', 'the audio element never started playing, so autoplay or the output device blocked the answer');
    } else if (playing.playbackPaused) {
      fail('playback_element_advancing', 'the audio element was paused while the answer was arriving');
    } else if (playing.playbackSeconds <= 0) {
      fail('playback_element_advancing', 'the audio element never advanced its playback position');
    } else {
      pass(
        'playback_element_advancing',
        `the local audio element played unpaused to ${playing.playbackSeconds.toFixed(2)}s; the output device, OS mixer, and speaker are past what this can observe`,
      );
    }

    // Barge-in is judged where `interrupt()` actually acts: the local element.
    // Inbound RTP can keep arriving and being decoded for packets already in
    // flight after playback has stopped, so failing on receiver energy would
    // condemn a barge-in that worked.
    const before = await session.audioProgress?.() ?? null;
    await wait(options.silenceWindowMs ?? 1_500);
    const after = await session.audioProgress?.() ?? null;
    if (!interrupted) {
      fail('barge_in', 'the interruption was never acknowledged');
    } else if (before === null || after === null) {
      fail('barge_in', 'the transport could not report the local playback element, so the stop is unproven');
    } else if (!after.playbackPaused) {
      fail('barge_in', 'the audio element was still playing after the interruption');
    } else if (after.playbackSeconds > before.playbackSeconds) {
      const advanced = (after.playbackSeconds - before.playbackSeconds).toFixed(2);
      fail('barge_in', `local playback kept advancing ${advanced}s over the ${options.silenceWindowMs ?? 1_500}ms after the interruption`);
    } else {
      const settling = before.audioEnergyReported && after.audioEnergy > before.audioEnergy
        ? '; inbound audio was still arriving, which is expected for packets already in flight'
        : '';
      pass(
        'barge_in',
        `local playback stopped at ${after.playbackSeconds.toFixed(2)}s and did not advance over the following ${options.silenceWindowMs ?? 1_500}ms${settling}`,
      );
    }
    if (audioEventsAfterInterrupt > 0) {
      fail('barge_in', `${audioEventsAfterInterrupt} further non-silent-audio events were attributed to the abandoned response`);
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

    // Reported last, so a run that ran out of patience still says what it
    // proved about every step rather than stopping the report at the timeout.
    if (timedOut) {
      throw new Error('Timed out waiting for the spoken exchange, host tool result, and follow-up answer.');
    }
    if (toolCallsBridged === 0) throw new Error('The provider never requested the host tool.');
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
