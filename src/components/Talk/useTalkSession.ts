/**
 * The browser half of Talk, as a hook.
 *
 * A microphone, a recorder, a level meter and a speaker, wired to
 * `talkEngine.ts`, which owns every decision. Nothing here decides when an
 * utterance ended or what may be spoken; it opens devices, moves bytes, and
 * reports what the engine says.
 *
 * It lives apart from the components that render it because a voice loop
 * duplicated across files is a voice loop that drifts. There used to be two
 * surfaces — a Talk page and the chat composer — and every difference between
 * them was a bug in one of them; now the composer is the only caller, and this
 * is still the only place that owns the devices.
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
  voiceRouteGet,
} from '../../lib/daemonClient';
import {
  BoundedPcmQueue,
  PCM_CAPTURE_WORKLET_URL,
  PcmRingBuffer,
  StreamingLinearResampler,
  base64AudioBlob,
  pcm16WavBlob,
  rmsOf,
} from '../../lib/talkAudio';
import { talkClient, type TalkStatus } from '../../lib/talkClient';
import { talkPlayer } from '../../lib/talkPlayback';
import { MicrophoneBlockedError, openMicrophone, type MicrophoneBlock } from '../../lib/microphoneAccess';
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
/**
 * How long an open microphone may deliver nothing before it has to explain
 * itself, and the level below which "nothing" is the honest word.
 *
 * A quiet room still reads around 1e-3; a dead capture path reads exactly
 * zero. The window is long enough to cover a device that takes a moment to
 * start and short enough that nobody sits watching a flat meter wondering.
 */
const SILENT_MICROPHONE_MS = 4_000;
const SILENT_MICROPHONE_RMS = 1e-4;

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
 * What makes two route records the same selection.
 *
 * `VoiceRouteSelector` calls `onRoute` on every refresh — a device appearing or
 * disappearing is enough — and hands back a freshly deserialised record for a
 * route that has not moved. Object identity therefore says "changed" far more
 * often than the route actually changes. The daemon's own identity is the route
 * id and its monotonic generation, and a stopped route is the same thing as no
 * route at all as far as this hook's devices are concerned.
 */
function routeIdentity(route: VoiceRouteRecord | null | undefined): string | null {
  return route && route.state === 'active' ? `${route.route_id}:${route.generation}` : null;
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
  /** Why the microphone is unavailable, when the answer has an action. */
  microphoneBlocked: MicrophoneBlock | null;
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
  /** Set when the refusal has a remedy, so the UI can offer it instead of prose. */
  const [microphoneBlocked, setMicrophoneBlocked] = useState<MicrophoneBlock | null>(null);
  const [grant, setGrant] = useState<CaptureGrant | null>(null);

  const sessionRef = useRef<TalkSession | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  /** Held for as long as the microphone is open — see `startRecording`. */
  const sourceRef = useRef<MediaStreamAudioSourceNode | null>(null);
  const workletRef = useRef<AudioWorkletNode | null>(null);
  const resamplerRef = useRef(new StreamingLinearResampler(KWS_SAMPLE_RATE));
  const ringRef = useRef(new PcmRingBuffer(KWS_RING_SAMPLES));
  const recordingPcmRef = useRef<Float32Array[] | null>(null);
  /**
   * Watches the first seconds of an open microphone for any sound at all.
   *
   * An open microphone that delivers nothing looks exactly like a quiet room:
   * the state badge says Listening, the meter sits at zero, and nothing
   * anywhere fails. Every cause behind that — a refused worklet module, a
   * suspended context, a muted track, a capture unit that hands back silence —
   * has been diagnosed by rebuilding the app with a print statement in it. The
   * microphone can say so itself instead.
   */
  const silenceWatchRef = useRef<{ frames: number; peak: number; timer: number } | null>(null);
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
  // Read inside the engine effect but deliberately not a dependency of it: the
  // composer changes this every time the operator flips push-to-talk, and
  // rebuilding the engine there would tear the microphone down and reopen it
  // mid-conversation. `mode` is already handled this way, through `setMode`.
  const autoStartModeRef = useRef(autoStartMode);
  autoStartModeRef.current = autoStartMode;
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
  /** The selection the route effect last acted on — see `routeIdentity`. */
  const routeIdentityRef = useRef<string | null>(null);
  /**
   * The highest route event that has already become conversation, with the
   * conversation it belongs to.
   *
   * The daemon's event ids are one monotonic sequence, so this alone is a
   * complete record of what has been handled — and a complete answer to the
   * spec's "never replay a physical capture as a new user turn if the durable
   * turn already exists". The cursor above is an optimization; this is the
   * guarantee, and it holds even if the cursor rewinds.
   */
  const handledEventRef = useRef<{ sessionId: string; eventId: number }>({ sessionId, eventId: 0 });
  const routeEmitQueueRef = useRef<Promise<unknown>>(Promise.resolve());
  const routedSpeechRef = useRef(new Map<string, { resolve: () => void; reject: (reason: Error) => void }>());
  const player = talkPlayer;

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
    if (silenceWatchRef.current) window.clearTimeout(silenceWatchRef.current.timer);
    silenceWatchRef.current = null;
    // The speaker goes with the microphone: the output was chosen inside the
    // gesture that opened this session, and it is not this one's to keep.
    player.release();
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
        try {
          streamRef.current = await openMicrophone({
            audio: deviceId
              ? { deviceId: { exact: deviceId }, echoCancellation: true, noiseSuppression: true }
              : { echoCancellation: true, noiseSuppression: true },
            video: false,
          });
        } catch (reason) {
          // Caught here because this is the last frame that still has the
          // error's identity: one below, `talkEngine` flattens it to
          // `reason.message` and the remedy goes with it.
          if (reason instanceof MicrophoneBlockedError) setMicrophoneBlocked(reason.block);
          throw reason;
        }
        // Still inside the press that opened the microphone, which is the only
        // moment WebKit will accept a sink. `config` is already in hand, so
        // this costs nothing extra.
        const routedOutput = localDevice(selected, 'output');
        outputDeviceRef.current =
          routedOutput && routedOutput !== 'default' ? routedOutput : config.voice.outputDeviceId;
        void player.setOutput(outputDeviceRef.current);
        for (const track of streamRef.current.getTracks()) {
          track.addEventListener?.('ended', () => sessionRef.current?.microphoneRevoked(), {
            once: true,
          });
        }
        // Built in the Talk button's own gesture when there is one — see
        // `openAudioContext`. Falling back to building it here keeps the
        // non-gesture callers (wake word rearming) working.
        const context = audioContextRef.current ?? new AudioContext();
        audioContextRef.current = context;
        // WebKit starts a context built outside a user gesture suspended, and a
        // suspended worklet receives no samples: the detector never hears an
        // utterance end, so Talk sits on "Listening" forever and nothing is
        // ever transcribed.
        if (context.state === 'suspended') await context.resume();
        // `resume()` resolving is not the same as the context running. Outside
        // a gesture WebKit settles the promise and leaves the state
        // `suspended`, and every failure downstream of that is silent: the
        // worklet is installed, the meter reads zero, the state badge sits on
        // "Listening", and nothing is ever transcribed. Say so instead.
        if (context.state === 'suspended') {
          streamRef.current.getTracks().forEach((track) => track.stop());
          streamRef.current = null;
          throw new Error(
            'The browser kept audio suspended, so the microphone produced no samples. Press Talk again.',
          );
        }
        if (!context.audioWorklet || typeof AudioWorkletNode === 'undefined') {
          streamRef.current.getTracks().forEach((track) => track.stop());
          streamRef.current = null;
          await context.close();
          throw new Error('This webview does not support the AudioWorklet PCM path required by Talk');
        }
        await context.audioWorklet.addModule(PCM_CAPTURE_WORKLET_URL);
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
        const watch = {
          frames: 0,
          peak: 0,
          timer: window.setTimeout(() => {
            const heard = silenceWatchRef.current;
            silenceWatchRef.current = null;
            if (!heard || heard.peak > SILENT_MICROPHONE_RMS) return;
            const track = streamRef.current?.getAudioTracks?.()[0];
            // Which of the two it is decides where to look next, so say which.
            setSetupError(
              heard.frames === 0
                ? `The microphone is open but no audio is arriving from it (device ${track?.label || 'unknown'}, `
                  + `track ${track?.readyState ?? 'missing'}${track?.muted ? ', muted' : ''}, `
                  + `audio ${context.state}). Talk cannot hear anything.`
                : `The microphone is open and delivering silence (device ${track?.label || 'unknown'}`
                  + `${track?.muted ? ', muted' : ''}, ${heard.frames} frames, peak ${heard.peak.toFixed(5)}). `
                  + 'Check that the right input is selected and that it is not muted.',
            );
          }, SILENT_MICROPHONE_MS),
        };
        silenceWatchRef.current = watch;
        worklet.port.onmessage = (event: MessageEvent<Float32Array | ArrayBuffer>) => {
          const raw = event.data instanceof Float32Array ? event.data : new Float32Array(event.data);
          const pcm = resamplerRef.current.process(raw, context.sampleRate);
          if (pcm.length === 0) return;
          ringRef.current.write(pcm);
          recordingPcmRef.current?.push(pcm.slice());
          const level = rmsOf(pcm);
          watch.frames += 1;
          if (level > watch.peak) watch.peak = level;
          sessionRef.current?.observeLevel(level);
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
        // The output was chosen when the microphone opened, inside the gesture
        // that asked for it — see `openPcmDevices`. Choosing it here instead
        // is what made the picker decorative: an answer arrives minutes after
        // any press, and WebKit refuses `setSinkId` without one.
        await player.play(base64AudioBlob(audioBase64, mediaType));
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

  /**
   * Build the AudioContext while the click that asked for Talk is still the
   * current gesture.
   *
   * WebKit decides whether a context may run from what is on the stack when it
   * is *constructed*, and `start` awaits the route activation, the permission
   * grant, the config read and `getUserMedia` before capture gets there. By
   * then the gesture is spent, the context is born suspended, `resume()`
   * settles without starting it, and the whole failure is silent — worklet
   * installed, meter at zero, badge on "Listening", nothing transcribed.
   *
   * Synchronous on purpose: one `await` above this line puts it back where it
   * was.
   */
  const openAudioContext = useCallback(() => {
    if (audioContextRef.current) return;
    try {
      audioContextRef.current = new AudioContext();
    } catch {
      // A webview that refuses to construct one at all fails later, in
      // `openPcmDevices`, where the message already explains itself.
    }
  }, []);

  // One engine per session. Rebuilt when the session changes, because a Talk
  // session belongs to exactly one conversation.
  useEffect(() => {
    if (!enabled) return;
    // Synchronous, and first: WebKit decides whether an AudioContext may run
    // from what is on the stack when it is *constructed*, and the `config()`
    // await below spends the click that enabled this hook. Without this the
    // composer's auto-started Talk builds a suspended context — meter at zero,
    // nothing transcribed, no error — which is the bug 78e45876 fixed for the
    // `start()` path only.
    openAudioContext();
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
        const auto = autoStartModeRef.current ?? (config.voice.alwaysListening ? 'continuous' : null);
        // Only a microphone the *setting* opened is the setting's to close. The
        // composer enables this hook when somebody presses Talk, so on that
        // path this is deliberately false and the teardown watcher below has
        // nothing to do: turning Always Listening off in Settings must not cut
        // off a conversation the operator started by hand.
        autoListeningRef.current = autoStartModeRef.current === null && config.voice.alwaysListening;
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
  }, [enabled, openAudioContext, ports, releaseDevices, sessionId]);

  useEffect(() => {
    routeRef.current = route;
    // Only a real change of selection may touch devices or rewind the cursor. A
    // re-fetch of the same route arrives as a different object several times a
    // session, and treating it as a change re-read this generation's events
    // from zero — replaying every transcript already spoken as another turn.
    const identity = routeIdentity(route);
    if (identity === routeIdentityRef.current) return;
    routeIdentityRef.current = identity;
    const external = Boolean(route?.state === 'active' && pairedInputDevice(route.input_endpoint));
    // Local capture closes here, before the paired endpoint is asked to own
    // one below: there must never be an interval where both microphones are
    // intentionally recording for this conversation.
    sessionRef.current?.setExternalInput(external);
    if (external) releaseDevices();
    routeCursorRef.current = route ? { generation: route.generation, eventId: 0 } : null;
    // A selector can move the route while Talk is already running. Selection
    // itself never opens a microphone; activation here preserves that privacy
    // boundary while making live handoff take effect immediately.
    if (route?.state === 'active' && snapshot?.state && snapshot.state !== 'off') {
      void activateAndWaitForRoute().catch(async (reason) => {
        setSetupError(errorMessage(reason));
        // Closing local capture first is only safe while a failed activation
        // still leaves a working microphone. `move_route` restores the previous
        // endpoints under a fresh generation when a handoff fails, so read that
        // authoritative record back and hand ownership to local capture again
        // whenever it no longer says a paired device holds it.
        if (!external) return;
        const restored = await voiceRouteGet(sessionId).catch(() => null);
        routeRef.current = restored;
        if (!restored || restored.state !== 'active' || !pairedInputDevice(restored.input_endpoint)) {
          sessionRef.current?.setExternalInput(false);
        }
      });
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
          const handled = handledEventRef.current;
          if (handled.sessionId === sessionId && event.event_id <= handled.eventId) continue;
          handledEventRef.current = { sessionId, eventId: event.event_id };
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
    openAudioContext();
    setSetupError(null);
    setMicrophoneBlocked(null);
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
  }, [activateAndWaitForRoute, openAudioContext, sessionId]);

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
   * read it can be open for hours — a conversation armed since breakfast is
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
          // The composer's "Always listening is on: the microphone is active"
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
    microphoneBlocked,
    start,
    stop,
    sessionRef,
  };
}
