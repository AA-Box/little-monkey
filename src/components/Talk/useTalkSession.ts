/**
 * The browser half of Talk, as a hook.
 *
 * A microphone, a recorder, a level meter and a speaker, wired to
 * `talkEngine.ts`, which owns every decision. Nothing here decides when an
 * utterance ended or what may be spoken; it opens devices, moves bytes, and
 * reports what the engine says.
 *
 * It lives apart from `TalkPanel` because the same conversation now runs from
 * two surfaces — the Talk panel and the chat composer's Talk button — and a
 * voice loop duplicated across two files is a voice loop that drifts.
 *
 * The turn itself is an ordinary one. `runAgentTurn(..., 'voice')` is the same
 * call the composer's Send makes, into the same session, with the same model
 * routing, tools, memory and approvals — so a spoken question and a typed one
 * land in one transcript and the operator can switch between them mid-thought.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

import { runAgentTurn, stopTurn } from '../../lib/agentLoop';
import { blobToBase64, companionClient, type CaptureGrant } from '../../lib/companionClient';
import { errorMessage } from '../../lib/errors';
import {
  type VoiceRouteRecord,
  voiceRouteActivate,
  voiceRouteDeactivate,
  voiceRouteEmit,
  voiceRouteEvents,
} from '../../lib/daemonClient';
import {
  BoundedPcmQueue,
  PCM_AUDIO_WORKLET_SOURCE,
  PcmRingBuffer,
  StreamingLinearResampler,
  base64AudioBlob,
  pcm16WavBlob,
  rmsOf,
} from '../../lib/talkAudio';
import { talkClient, type TalkStatus } from '../../lib/talkClient';
import { createTalkPlayer } from '../../lib/talkPlayback';
import {
  TalkSession,
  type TalkMode,
  type TalkPorts,
  type TalkRecording,
  type TalkSnapshot,
} from '../../lib/talkEngine';
import { useSessionStore } from '../../store/sessionStore';

/** Long enough for a conversation, short enough that a forgotten tab expires. */
const GRANT_LIFETIME_MS = 30 * 60_000;
const KWS_SAMPLE_RATE = 16_000;
const KWS_RING_SAMPLES = KWS_SAMPLE_RATE * 3;
const KWS_PENDING_SAMPLES = KWS_SAMPLE_RATE / 5;

function joinPcm(chunks: readonly Float32Array[]): Float32Array {
  const length = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const output = new Float32Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.length;
  }
  return output;
}

/**
 * How the durable routes mark text they park in the answer's place.
 *
 * A daemon-routed turn writes its progress into the message the answer will
 * eventually occupy — "Queued in the resident runner…", "Resident agent is
 * working…", "Preparing read_file…", "Waiting for approval: …", and the rest of
 * `projectDaemonTurnEvents`'s status line. On screen that is exactly right. Read
 * aloud it is the plumbing narrating itself, before every single answer.
 *
 * Matching one exact sentence caught only the queue placeholder; the one that
 * actually reaches most turns is "Resident agent is working…", because the
 * `started` event fires on all of them. They share this marker, so skip on the
 * marker rather than on a list of sentences that will grow again.
 */
const PLACEHOLDER_MARKER = '⏳';

/**
 * How much of the conversation is offered to the recognizer as vocabulary.
 *
 * whisper.cpp conditions on the *tail* of this text, the same way it conditions
 * on the previous window when transcribing a long recording, so the newest
 * words are the ones that count and there is nothing to gain from sending the
 * whole transcript.
 */
const CONTEXT_LIMIT = 800;

/**
 * The words this conversation has already used, for the recognizer to expect.
 *
 * A proper noun the model has never seen comes back as whatever it sounds like
 * in the language it detected — "Sundbyberg" as "soon the B-Berry". Once the
 * name is on screen, offering it back means the next utterance is decoded
 * against a vocabulary that contains it.
 *
 * Placeholders are left out: priming the decoder with "Queued in the resident
 * runner" teaches it the plumbing's words, not the conversation's.
 */
function recentConversation(sessionId: string): string | null {
  const session = useSessionStore.getState().sessions.find((entry) => entry.id === sessionId);
  if (!session) return null;
  const text = session.messages
    .slice(-6)
    .map((message) => (typeof message.content === 'string' ? message.content : ''))
    .filter((value) => value && !value.startsWith(PLACEHOLDER_MARKER))
    .join(' ')
    .replace(/\s+/g, ' ')
    .trim();
  return text ? text.slice(-CONTEXT_LIMIT) : null;
}

export interface UseTalkSessionOptions {
  /**
   * Whether to build an engine at all. `false` keeps this hook inert — no
   * config read, no engine, no devices — which is what the composer wants
   * before anybody has asked for Talk.
   */
  enabled?: boolean;
  /** Start in this mode as soon as the engine exists. */
  autoStartMode?: TalkMode | null;
  /** Host-authoritative route selected for this ordinary conversation. */
  route?: VoiceRouteRecord | null;
}

export interface UseTalkSession {
  snapshot: TalkSnapshot | null;
  status: TalkStatus | null;
  setStatus: (status: TalkStatus | null) => void;
  mode: TalkMode;
  setMode: (mode: TalkMode) => void;
  setupError: string | null;
  setSetupError: (message: string | null) => void;
  start: () => Promise<void>;
  stop: () => Promise<void>;
  /** The live engine, for push-to-talk and the Stop button. */
  sessionRef: React.RefObject<TalkSession | null>;
}

export function useTalkSession(
  sessionId: string,
  { enabled = true, autoStartMode = null, route = null }: UseTalkSessionOptions = {},
): UseTalkSession {
  const [snapshot, setSnapshot] = useState<TalkSnapshot | null>(null);
  const [status, setStatus] = useState<TalkStatus | null>(null);
  const [mode, setMode] = useState<TalkMode>('push_to_talk');
  const [setupError, setSetupError] = useState<string | null>(null);
  const [grant, setGrant] = useState<CaptureGrant | null>(null);

  const sessionRef = useRef<TalkSession | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  /** Held for as long as the microphone is open — see `startRecording`. */
  const sourceRef = useRef<MediaStreamAudioSourceNode | null>(null);
  const workletRef = useRef<AudioWorkletNode | null>(null);
  const workletUrlRef = useRef<string | null>(null);
  const resamplerRef = useRef(new StreamingLinearResampler(KWS_SAMPLE_RATE));
  const ringRef = useRef(new PcmRingBuffer(KWS_RING_SAMPLES));
  const recordingPcmRef = useRef<Float32Array[] | null>(null);
  const wakeSessionRef = useRef<{ sessionId: string; startSample: number } | null>(null);
  const wakeQueueRef = useRef(new BoundedPcmQueue(KWS_PENDING_SAMPLES));
  const wakePushBusyRef = useRef(false);
  /**
   * True while this session is listening *because of the setting* rather than
   * because somebody pressed something.
   *
   * That distinction is the whole authority question. A press is the operator
   * asking for a microphone, and revoking Always Listening does not retract a
   * press. Always Listening is the operator asking for one they never have to
   * press for — so when it goes away, the microphone it opened has to go with
   * it, from whichever surface it was switched off on.
   */
  const autoListeningRef = useRef(false);
  const grantRef = useRef<CaptureGrant | null>(null);
  /**
   * The turn Talk is waiting on: the utterance id the durable ingress was given,
   * where in the transcript its answer will appear, and how much of that answer
   * has already been handed to the engine.
   *
   * Bound to the submitted turn rather than to "the last assistant message",
   * which is a different thing whenever anything else touches the session — a
   * message typed in the composer, a completed answer the store mutates again,
   * or the run this one interrupted.
   */
  const activeTurnRef = useRef<{ turnId: string; fromIndex: number; spoken: string } | null>(null);
  /** The last output device successfully read from settings. */
  const outputDeviceRef = useRef<string | null>(null);
  const routeRef = useRef<VoiceRouteRecord | null>(route);
  const routeCursorRef = useRef<{ generation: number; eventId: number } | null>(null);
  const routeEmitQueueRef = useRef<Promise<unknown>>(Promise.resolve());
  const routedSpeechRef = useRef(new Map<string, { resolve: () => void; reject: (reason: Error) => void }>());
  const player = useMemo(() => createTalkPlayer(), []);

  const pairedInputDevice = (value: string | undefined | null) => {
    const match = value?.match(/^paired:(.+):input$/);
    return match?.[1] ?? null;
  };
  const pairedOutputDevice = (value: string | undefined | null) => {
    const match = value?.match(/^paired:(.+):output$/);
    return match?.[1] ?? null;
  };
  const localDevice = (value: string | undefined | null, direction: 'input' | 'output') => {
    const prefix = `local:${direction}:`;
    return value?.startsWith(prefix) ? value.slice(prefix.length) || 'default' : null;
  };
  const emitRoute = (
    currentRoute: VoiceRouteRecord,
    kind: string,
    payload: unknown,
  ) => {
    routeEmitQueueRef.current = routeEmitQueueRef.current
      .catch(() => undefined)
      .then(() => voiceRouteEmit(sessionId, currentRoute.generation, kind, payload))
      .catch(() => undefined);
  };

  const activateAndWaitForRoute = useCallback(async (): Promise<VoiceRouteRecord | null> => {
    const selected = routeRef.current;
    if (!selected || selected.state !== 'active') return selected;
    const activated = await voiceRouteActivate(sessionId);
    routeRef.current = activated;
    const required = new Map<string, 'input_ready' | 'output_ready'>();
    if (pairedInputDevice(activated.input_endpoint) && activated.input_command_id) {
      required.set(activated.input_command_id, 'input_ready');
    }
    const pairedOutput = pairedOutputDevice(activated.output_endpoint);
    if (pairedOutput
        && pairedInputDevice(activated.input_endpoint) !== pairedOutput
        && activated.output_command_id) {
      required.set(activated.output_command_id, 'output_ready');
    }
    if (required.size === 0) return activated;
    let cursor = routeCursorRef.current?.generation === activated.generation
      ? routeCursorRef.current.eventId
      : 0;
    const deadline = Date.now() + 20_000;
    while (required.size > 0 && Date.now() < deadline) {
      const events = await voiceRouteEvents(sessionId, cursor, 100);
      for (const event of events) {
        cursor = Math.max(cursor, event.event_id);
        if (event.generation !== activated.generation || !event.payload || typeof event.payload !== 'object') continue;
        const payload = event.payload as Record<string, unknown>;
        const commandId = typeof payload.command_id === 'string' ? payload.command_id : '';
        if (commandId && required.get(commandId) === event.kind) required.delete(commandId);
      }
      routeCursorRef.current = { generation: activated.generation, eventId: cursor };
      if (required.size > 0) await new Promise((resolve) => setTimeout(resolve, 150));
    }
    if (required.size > 0) {
      throw new Error('A paired VoiceRoute endpoint did not become ready within 20 seconds');
    }
    return activated;
  }, [sessionId]);

  useEffect(() => {
    grantRef.current = grant;
  }, [grant]);

  useEffect(() => {
    // Gated with everything else: a chat window that nobody has asked to speak
    // to should cost no IPC at all, and there is nothing to report about a
    // backend until somebody wants to use it.
    if (!enabled) return;
    void talkClient
      .status()
      .then(setStatus)
      .catch((reason) => setSetupError(errorMessage(reason)));
  }, [enabled]);

  const ensureGrant = useCallback(async (): Promise<CaptureGrant> => {
    const current = grantRef.current;
    if (current && current.active && current.expiresAtMs > Date.now()) return current;
    const fresh = await companionClient.grant('microphone', GRANT_LIFETIME_MS, 'talk');
    grantRef.current = fresh;
    setGrant(fresh);
    return fresh;
  }, []);

  /** Close the microphone and every node hanging off it. Safe to call twice. */
  const releaseDevices = useCallback(() => {
    const wake = wakeSessionRef.current;
    wakeSessionRef.current = null;
    if (wake) {
      void talkClient
        .wakeWordStop(wake.sessionId, wakeQueueRef.current.droppedFrames)
        .catch(() => undefined);
    }
    recordingPcmRef.current = null;
    workletRef.current?.port.close();
    workletRef.current?.disconnect();
    workletRef.current = null;
    sourceRef.current?.disconnect();
    sourceRef.current = null;
    streamRef.current?.getTracks().forEach((track) => track.stop());
    streamRef.current = null;
    void audioContextRef.current?.close().catch(() => undefined);
    audioContextRef.current = null;
    resamplerRef.current.reset();
    ringRef.current = new PcmRingBuffer(KWS_RING_SAMPLES);
    wakeQueueRef.current = new BoundedPcmQueue(KWS_PENDING_SAMPLES);
    wakePushBusyRef.current = false;
    if (workletUrlRef.current) URL.revokeObjectURL(workletUrlRef.current);
    workletUrlRef.current = null;
    const activeGrant = grantRef.current;
    grantRef.current = null;
    setGrant(null);
    if (activeGrant) void companionClient.revoke(activeGrant.grantId).catch(() => undefined);
  }, []);

  const ports = useMemo<TalkPorts>(() => {
    const pumpWakeQueue = async (): Promise<void> => {
      if (wakePushBusyRef.current) return;
      const wake = wakeSessionRef.current;
      const activeGrant = grantRef.current;
      const frame = wakeQueueRef.current.take();
      if (!wake || !activeGrant || frame.length === 0) return;
      wakePushBusyRef.current = true;
      try {
        const detection = await talkClient.wakeWordPush(
          activeGrant.grantId,
          wake.sessionId,
          frame,
        );
        if (detection && wakeSessionRef.current?.sessionId === detection.sessionId) {
          // Stop forwarding immediately and close the native generation before
          // command capture. Clearing the ref first makes a rapid duplicate
          // response inert; the explicit stop ensures native state does not
          // remain accepting after that clear.
          wakeSessionRef.current = null;
          await talkClient
            .wakeWordStop(detection.sessionId, wakeQueueRef.current.droppedFrames)
            .catch(() => false);
          const commandStart = wake.startSample + detection.keywordEndSample;
          await sessionRef.current?.onWakeDetected(commandStart);
        }
      } catch (reason) {
        // A stopped generation can still have one IPC response in flight. It
        // is stale, not a session error.
        if (wakeSessionRef.current?.sessionId === wake.sessionId) {
          wakeSessionRef.current = null;
          sessionRef.current?.wakeWordFailed(errorMessage(reason));
        }
      } finally {
        wakePushBusyRef.current = false;
        if (wakeSessionRef.current && wakeQueueRef.current.length > 0) void pumpWakeQueue();
      }
    };

    const openPcmDevices = async () => {
      if (!streamRef.current) {
        const grantForCapture = await ensureGrant();
        void grantForCapture;
        const config = await companionClient.config();
        const selected = routeRef.current?.state === 'active'
          ? routeRef.current.input_endpoint
          : null;
        if (pairedInputDevice(selected)) {
          throw new Error('The paired VoiceRoute endpoint owns microphone capture for this Talk session');
        }
        const routedLocal = localDevice(selected, 'input');
        const deviceId = (routedLocal && routedLocal !== 'default' ? routedLocal : config.voice.inputDeviceId) ?? undefined;
        streamRef.current = await navigator.mediaDevices.getUserMedia({
          audio: deviceId
            ? { deviceId: { exact: deviceId }, echoCancellation: true, noiseSuppression: true }
            : { echoCancellation: true, noiseSuppression: true },
          video: false,
        });
        for (const track of streamRef.current.getTracks()) {
          track.addEventListener?.('ended', () => sessionRef.current?.microphoneRevoked(), {
            once: true,
          });
        }
        const context = new AudioContext();
        // WebKit starts a context built outside a user gesture suspended, and a
        // suspended worklet receives no samples: the detector never hears an
        // utterance end, so Talk sits on "Listening" forever and nothing is
        // ever transcribed. The Talk button *is* a gesture, but the awaits
        // above — the grant, the config read, `getUserMedia` — have spent it by
        // the time the context exists. A refusal here is not silently ignored:
        // it fails `startRecording`, and the engine says so rather than
        // claiming to be listening with a dead meter.
        if (context.state === 'suspended') await context.resume();
        if (!context.audioWorklet || typeof AudioWorkletNode === 'undefined') {
          streamRef.current.getTracks().forEach((track) => track.stop());
          streamRef.current = null;
          await context.close();
          throw new Error('This webview does not support the AudioWorklet PCM path required by Talk');
        }
        const workletUrl = URL.createObjectURL(
          new Blob([PCM_AUDIO_WORKLET_SOURCE], { type: 'text/javascript' }),
        );
        workletUrlRef.current = workletUrl;
        await context.audioWorklet.addModule(workletUrl);
        const worklet = new AudioWorkletNode(context, 'little-monkey-pcm-capture', {
          numberOfInputs: 1,
          numberOfOutputs: 0,
          channelCount: 1,
        });
        const source = context.createMediaStreamSource(streamRef.current);
        source.connect(worklet);
        sourceRef.current = source;
        workletRef.current = worklet;
        audioContextRef.current = context;
        worklet.port.onmessage = (event: MessageEvent<Float32Array | ArrayBuffer>) => {
          const raw = event.data instanceof Float32Array ? event.data : new Float32Array(event.data);
          const pcm = resamplerRef.current.process(raw, context.sampleRate);
          if (pcm.length === 0) return;
          ringRef.current.write(pcm);
          recordingPcmRef.current?.push(pcm.slice());
          sessionRef.current?.observeLevel(rmsOf(pcm));
          if (wakeSessionRef.current) {
            wakeQueueRef.current.enqueue(pcm);
            void pumpWakeQueue();
          }
        };
      }
    };

    const startRecording = async (options?: { afterSample?: number }) => {
      await openPcmDevices();
      const buffered = options?.afterSample === undefined
        ? new Float32Array()
        : ringRef.current.sliceFrom(options.afterSample);
      recordingPcmRef.current = buffered.length > 0 ? [buffered] : [];
    };

    const stopRecording = async (): Promise<TalkRecording | null> => {
      const chunks = recordingPcmRef.current;
      recordingPcmRef.current = null;
      if (!chunks) return null;
      const pcm = joinPcm(chunks);
      if (pcm.length === 0) return null;
      const blob = pcm16WavBlob(pcm, KWS_SAMPLE_RATE);
      return { blob, mediaType: blob.type };
    };

    return {
      startRecording,
      stopRecording,
      armWakeWord: async () => {
        await openPcmDevices();
        const active = await ensureGrant();
        const started = await talkClient.wakeWordStart(active.grantId);
        wakeQueueRef.current = new BoundedPcmQueue(KWS_PENDING_SAMPLES);
        wakeSessionRef.current = {
          sessionId: started.sessionId,
          startSample: ringRef.current.totalWritten,
        };
        setStatus((current) => current ? { ...current, wakeWord: started.status } : current);
      },
      disarmWakeWord: async () => {
        const wake = wakeSessionRef.current;
        wakeSessionRef.current = null;
        if (!wake) return;
        await talkClient
          .wakeWordStop(wake.sessionId, wakeQueueRef.current.droppedFrames)
          .catch(() => false);
      },
      now: () => Date.now(),
      transcribe: async (recording, jobId) => {
        const active = await ensureGrant();
        const audioBase64 = await blobToBase64(recording.blob);
        // Talk's own transcription, not the companion's: that one publishes the
        // transcript, and the raw audio too when the operator asked for
        // artifacts. A spoken conversation is not a recording somebody asked to
        // keep, so this path holds the bytes for the length of the call and
        // publishes nothing.
        const result = await talkClient.transcribe(
          active.grantId,
          jobId,
          audioBase64,
          recording.mediaType,
          recentConversation(sessionId),
        );
        return result.text;
      },
      submitTurn: async (text, utteranceId) => {
        // The composer's own call. `voice` only labels where the turn was made.
        // A paired microphone enters here too, so both sources are literally the
        // same ordinary conversation execution path.
        const session = useSessionStore
          .getState()
          .sessions.find((entry) => entry.id === sessionId);
        activeTurnRef.current = {
          turnId: utteranceId,
          fromIndex: session?.messages.length ?? 0,
          spoken: '',
        };
        let failure: string | null = null;
        try {
          await runAgentTurn(sessionId, text, [], undefined, utteranceId, [], [], false, null, 'voice');
        } catch (reason) {
          failure = errorMessage(reason);
          throw reason;
        } finally {
          const currentRoute = routeRef.current;
          if (currentRoute?.state === 'active' && pairedInputDevice(currentRoute.input_endpoint)) {
            emitRoute(
              currentRoute,
              failure ? 'turn_failed' : 'turn_finished',
              failure ? { turn_id: utteranceId, error: failure } : { turn_id: utteranceId },
            );
          }
          if (activeTurnRef.current?.turnId === utteranceId) {
            activeTurnRef.current = null;
            sessionRef.current?.onTurnFinished(utteranceId, failure ?? undefined);
          }
        }
      },
      cancelTurn: () => stopTurn(sessionId),
      speakText: async (text, jobId) => {
        const currentRoute = routeRef.current;
        const pairedOutput = pairedOutputDevice(currentRoute?.output_endpoint);
        if (!currentRoute || currentRoute.state !== 'active' || !pairedOutput) return false;
        // A same-device duplex route already synthesizes the same assistant
        // deltas on the input Talk socket. Do not emit a second speech job.
        if (pairedInputDevice(currentRoute.input_endpoint) === pairedOutput) return true;
        const completion = new Promise<void>((resolve, reject) => {
          routedSpeechRef.current.set(jobId, { resolve, reject });
        });
        try {
          await voiceRouteEmit(sessionId, currentRoute.generation, 'speak_text', {
            job_id: jobId,
            turn_id: activeTurnRef.current?.turnId ?? '',
            text,
          });
          await Promise.race([
            completion,
            new Promise<void>((_, reject) => setTimeout(() => reject(new Error('Paired speaker did not acknowledge playback')), 90_000)),
          ]);
          return true;
        } finally {
          routedSpeechRef.current.delete(jobId);
        }
      },
      synthesize: async (text, jobId) => {
        const speech = await talkClient.synthesize(jobId, text);
        return { audioBase64: speech.audioBase64, mediaType: speech.mediaType };
      },
      play: async (audioBase64, mediaType) => {
        const currentRoute = routeRef.current;
        try {
          const configured = (await companionClient.config()).voice.outputDeviceId;
          const routedLocal = localDevice(currentRoute?.output_endpoint, 'output');
          outputDeviceRef.current = routedLocal && routedLocal !== 'default' ? routedLocal : configured;
        } catch {
          /* keep the last known output */
        }
        await player.play(base64AudioBlob(audioBase64, mediaType), outputDeviceRef.current);
      },
      stopPlayback: () => {
        player.stop();
        const currentRoute = routeRef.current;
        const pairedOutput = pairedOutputDevice(currentRoute?.output_endpoint);
        if (currentRoute?.state === 'active'
            && pairedOutput
            && pairedInputDevice(currentRoute.input_endpoint) !== pairedOutput) {
          emitRoute(currentRoute, 'output_stop', { reason: 'host_interrupt' });
        }
        for (const pending of routedSpeechRef.current.values()) {
          pending.reject(new Error('Playback interrupted'));
        }
        routedSpeechRef.current.clear();
      },
      recordMetric: (metric) => {
        void talkClient.recordMetric(metric).catch(() => undefined);
      },
    };
  }, [ensureGrant, player, sessionId]);

  // One engine per session. Rebuilt when the session changes, because a Talk
  // session belongs to exactly one conversation.
  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    let engine: TalkSession | null = null;
    void companionClient
      .config()
      .then((config) => {
        if (disposed) return;
        engine = new TalkSession(ports, {
          mode,
          vad: {
            minSpeechMs: config.voice.vadMinSpeechMs,
            silenceMs: config.voice.vadSilenceMs,
            maxUtteranceMs: config.voice.vadMaxUtteranceMs,
          },
          // Phrase compilation, runtime readiness and audio acceptance all live
          // behind the native boundary. The engine owns only the transition
          // from an authenticated wake event into ordinary command capture.
          wakeWordEnabled: config.voice.wakePhraseEnabled,
        });
        sessionRef.current = engine;
        engine.subscribe(setSnapshot);
        // The always-listening setting's entire claim is that Talk listens for
        // as long as it is open, without anyone pressing Start. Continuous is
        // the shape that makes that true, and the wake phrase — which the Rust
        // side refuses to let this setting exist without — is what decides
        // whether anything heard is submitted. `autoStartMode` is the same
        // thing asked for directly, by a caller that only enables this hook
        // once somebody has pressed something. Either way the microphone opens
        // no earlier than this, and closes with the surface that opened it:
        // there is no listening behind the operator's back.
        const auto = autoStartMode ?? (config.voice.alwaysListening ? 'continuous' : null);
        autoListeningRef.current = autoStartMode === null && config.voice.alwaysListening;
        if (auto) {
          setMode(auto);
          engine.setMode(auto);
          void engine.start();
        }
      })
      .catch((reason) => {
        if (!disposed) setSetupError(errorMessage(reason));
      });
    return () => {
      disposed = true;
      void engine?.stop();
      sessionRef.current = null;
      // A turn belongs to the conversation it was asked in. Leaving it here
      // would point the next session's watcher at an index in the last one's
      // transcript.
      activeTurnRef.current = null;
      autoListeningRef.current = false;
      releaseDevices();
    };
    // `ports` is memoized on the session; `mode` is applied through `setMode`
    // below rather than by rebuilding the engine mid-conversation.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoStartMode, enabled, ports, releaseDevices, sessionId]);

  useEffect(() => {
    routeRef.current = route;
    const external = Boolean(route?.state === 'active' && pairedInputDevice(route.input_endpoint));
    sessionRef.current?.setExternalInput(external);
    if (external) releaseDevices();
    routeCursorRef.current = route ? { generation: route.generation, eventId: 0 } : null;
    // A selector can move the route while Talk is already running. Selection
    // itself never opens a microphone; activation here preserves that privacy
    // boundary while making live handoff take effect immediately.
    if (route?.state === 'active' && snapshot?.state && snapshot.state !== 'off') {
      void activateAndWaitForRoute().catch((reason) => setSetupError(errorMessage(reason)));
    }
  }, [activateAndWaitForRoute, releaseDevices, route, sessionId]);

  useEffect(() => {
    if (!enabled || snapshot?.state === 'off' || !route || route.state !== 'active'
        || (!pairedInputDevice(route.input_endpoint) && !pairedOutputDevice(route.output_endpoint))) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      try {
        const cursor = routeCursorRef.current?.generation === route.generation
          ? routeCursorRef.current.eventId
          : 0;
        const events = await voiceRouteEvents(sessionId, cursor, 100);
        if (disposed) return;
        let next = cursor;
        for (const event of events) {
          next = Math.max(next, event.event_id);
          if (event.generation !== route.generation || typeof event.payload !== 'object' || !event.payload) continue;
          const payload = event.payload as Record<string, unknown>;
          if (event.kind === 'input_transcript') {
            const text = typeof payload.text === 'string' ? payload.text : '';
            const turnId = typeof payload.turn_id === 'string' ? payload.turn_id : '';
            if (text && turnId) {
              void sessionRef.current?.acceptExternalTranscript(text, turnId, {
                speechDetectionMs: typeof payload.speech_detection_ms === 'number' ? payload.speech_detection_ms : null,
                sttMs: typeof payload.stt_ms === 'number' ? payload.stt_ms : null,
              });
            }
          } else if (event.kind === 'interrupt') {
            sessionRef.current?.interrupt(
              typeof payload.reason === 'string' ? payload.reason : 'remote_barge_in',
            );
          } else if (event.kind === 'output_played' || event.kind === 'output_failed') {
            const jobId = typeof payload.job_id === 'string' ? payload.job_id : '';
            const pending = jobId ? routedSpeechRef.current.get(jobId) : undefined;
            if (pending) {
              if (event.kind === 'output_played') pending.resolve();
              else pending.reject(new Error(typeof payload.error === 'string' ? payload.error : 'Paired speaker playback failed'));
              routedSpeechRef.current.delete(jobId);
            }
          }
        }
        routeCursorRef.current = { generation: route.generation, eventId: next };
      } catch (reason) {
        if (!disposed) setSetupError(errorMessage(reason));
      } finally {
        if (!disposed) timer = setTimeout(() => void poll(), 180);
      }
    };
    void poll();
    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
    };
  }, [enabled, route, sessionId, snapshot?.state]);

  useEffect(() => {
    sessionRef.current?.setMode(mode);
  }, [mode]);

  /**
   * The assistant's answer, as the ordinary session produces it.
   *
   * Read from the session store rather than from a Talk-specific stream: the
   * spoken turn and a typed one are the same turn, and there is only one
   * transcript. What is spoken is only ever the answer to the turn Talk itself
   * submitted — the last assistant message is somebody else's whenever the
   * operator has also typed something, and a finished answer is still the last
   * one long after it was read out.
   */
  useEffect(() => {
    return useSessionStore.subscribe((store) => {
      const engine = sessionRef.current;
      const active = activeTurnRef.current;
      if (!engine || !active) return;
      const session = store.sessions.find((entry) => entry.id === sessionId);
      if (!session) return;
      // The turn's LAST assistant message, not its first. A turn that calls a
      // tool writes one assistant message per round, and the first of them is
      // the one that requested the tool — usually with empty content. Reading
      // the first left every tool-using turn silent: the answer arrives in a
      // later message the watcher never looked at.
      let answer: (typeof session.messages)[number] | undefined;
      for (let index = session.messages.length - 1; index >= active.fromIndex; index -= 1) {
        if (session.messages[index].role === 'assistant') {
          answer = session.messages[index];
          break;
        }
      }
      if (!answer) return;
      const text = typeof answer.content === 'string' ? answer.content : '';
      // A real answer that opens with an hourglass would be skipped too. That
      // costs one unspoken turn; the alternative costs every turn a preamble.
      if (!text || text.startsWith(PLACEHOLDER_MARKER)) return;
      // Both routes replace the message's content rather than appending to it,
      // so "longer than last time" is not the same question as "continues what
      // has already been spoken". When it does not continue it, the message was
      // rewritten and none of what is there now has been said out loud.
      const delta = text.startsWith(active.spoken) ? text.slice(active.spoken.length) : text;
      active.spoken = text;
      if (delta) {
        engine.onAssistantDelta(delta, active.turnId);
        const currentRoute = routeRef.current;
        if (currentRoute?.state === 'active' && pairedInputDevice(currentRoute.input_endpoint)) {
          emitRoute(currentRoute, 'assistant_delta', { turn_id: active.turnId, text: delta });
        }
      }
    });
  }, [sessionId]);

  const start = useCallback(async () => {
    setSetupError(null);
    try {
      const currentRoute = routeRef.current;
      sessionRef.current?.setExternalInput(Boolean(
        currentRoute?.state === 'active' && pairedInputDevice(currentRoute.input_endpoint),
      ));
      if (currentRoute?.state === 'active') await activateAndWaitForRoute();
      await sessionRef.current?.start();
    } catch (reason) {
      setSetupError(errorMessage(reason));
    }
  }, [activateAndWaitForRoute, sessionId]);

  const stop = useCallback(async () => {
    await sessionRef.current?.stop();
    await voiceRouteDeactivate(sessionId).catch(() => null);
    releaseDevices();
  }, [releaseDevices, sessionId]);

  /**
   * Turning Always Listening off closes the microphone it opened. Here, not in
   * the switch.
   *
   * The setting is read once, when the engine is built, and the surface that
   * read it can be open for hours — a Talk panel armed since breakfast is
   * exactly the session an operator goes to Settings to turn off. Reacting in
   * the Settings switch, or in the panel's own "Stop listening" button, only
   * covers the surface that happens to hold the switch; every other route to
   * the same save — the other panel, an imported configuration, a second
   * window — left a live microphone behind. So the reaction lives in the hook
   * that owns the devices, and every route to a saved configuration reaches it.
   *
   * `stop()` is the same one the Stop button calls: engine to `off`, native KWS
   * generation closed, tracks stopped, worklet and context torn down, ring and
   * wake queue replaced, grant revoked.
   *
   * Only the microphone the *setting* opened is closed — `autoListeningRef` —
   * and only turning it off closes anything. Turning it *on* here would open a
   * microphone from a background event, and the engine on screen was built
   * without wake gating, so it would be an ungated one: that stays a decision
   * the next mount makes, with the wake word compiled in.
   */
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const subscription = companionClient.onConfigChanged(() => {
      if (cancelled || !autoListeningRef.current) return;
      void companionClient
        .config()
        .then(async (config) => {
          if (cancelled || !autoListeningRef.current || config.voice.alwaysListening) return;
          autoListeningRef.current = false;
          await stop();
          // The Talk panel's "Always listening is on: the microphone is active"
          // banner is drawn from `TalkStatus`, which was read when the surface
          // opened. Leaving it stale would put a claim that the microphone is
          // active directly above a microphone this just closed.
          await talkClient.status().then(setStatus).catch(() => undefined);
        })
        .catch((reason) => {
          // A configuration that cannot be read is not a reason to keep a
          // microphone open on the strength of a stale copy of it.
          if (cancelled) return;
          autoListeningRef.current = false;
          setSetupError(errorMessage(reason));
          void stop();
        });
    });
    return () => {
      cancelled = true;
      void subscription.then((unlisten) => unlisten()).catch(() => undefined);
    };
  }, [enabled, stop]);

  return {
    snapshot,
    status,
    setStatus,
    mode,
    setMode,
    setupError,
    setSetupError,
    start,
    stop,
    sessionRef,
  };
}
