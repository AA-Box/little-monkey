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
import { base64AudioBlob } from '../../lib/talkAudio';
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
/** How often the level meter samples the microphone. Matches the VAD's frame. */
const METER_INTERVAL_MS = 20;

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
  { enabled = true, autoStartMode = null }: UseTalkSessionOptions = {},
): UseTalkSession {
  const [snapshot, setSnapshot] = useState<TalkSnapshot | null>(null);
  const [status, setStatus] = useState<TalkStatus | null>(null);
  const [mode, setMode] = useState<TalkMode>('push_to_talk');
  const [setupError, setSetupError] = useState<string | null>(null);
  const [grant, setGrant] = useState<CaptureGrant | null>(null);

  const sessionRef = useRef<TalkSession | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const chunksRef = useRef<Blob[]>([]);
  const audioContextRef = useRef<AudioContext | null>(null);
  const analyserRef = useRef<AnalyserNode | null>(null);
  /** Held for as long as the microphone is open — see `startRecording`. */
  const sourceRef = useRef<MediaStreamAudioSourceNode | null>(null);
  const meterRef = useRef<number | null>(null);
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
  const player = useMemo(() => createTalkPlayer(), []);

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
    if (meterRef.current !== null) {
      window.clearInterval(meterRef.current);
      meterRef.current = null;
    }
    recorderRef.current = null;
    analyserRef.current = null;
    sourceRef.current?.disconnect();
    sourceRef.current = null;
    streamRef.current?.getTracks().forEach((track) => track.stop());
    streamRef.current = null;
    void audioContextRef.current?.close().catch(() => undefined);
    audioContextRef.current = null;
  }, []);

  const ports = useMemo<TalkPorts>(() => {
    const startRecording = async () => {
      const grantForCapture = await ensureGrant();
      void grantForCapture;
      if (!streamRef.current) {
        const config = await companionClient.config();
        const deviceId = config.voice.inputDeviceId ?? undefined;
        streamRef.current = await navigator.mediaDevices.getUserMedia({
          audio: deviceId
            ? { deviceId: { exact: deviceId }, echoCancellation: true, noiseSuppression: true }
            : { echoCancellation: true, noiseSuppression: true },
          video: false,
        });
        // The meter reads levels, never samples anybody else can see: the
        // analyser's output is one number per frame and it goes straight into
        // the detector.
        const context = new AudioContext();
        // WebKit starts a context built outside a user gesture suspended, and a
        // suspended analyser reads pure silence: the detector never hears an
        // utterance end, so Talk sits on "Listening" forever and nothing is
        // ever transcribed. The Talk button *is* a gesture, but the awaits
        // above — the grant, the config read, `getUserMedia` — have spent it by
        // the time the context exists. A refusal here is not silently ignored:
        // it fails `startRecording`, and the engine says so rather than
        // claiming to be listening with a dead meter.
        if (context.state === 'suspended') await context.resume();
        const analyser = context.createAnalyser();
        analyser.fftSize = 1024;
        // The source node is held, not dropped on the floor. WebKit collects a
        // `MediaStreamAudioSourceNode` nothing references, and the analyser it
        // fed then reads pure silence forever — a flat meter, a detector that
        // never hears an utterance start or end, and Talk stuck on "Listening"
        // while the recorder happily records. Chromium keeps it alive on the
        // graph, which is why this only ever showed up in the desktop webview.
        const source = context.createMediaStreamSource(streamRef.current);
        source.connect(analyser);
        sourceRef.current = source;
        audioContextRef.current = context;
        analyserRef.current = analyser;
        const buffer = new Float32Array(analyser.fftSize);
        meterRef.current = window.setInterval(() => {
          const active = analyserRef.current;
          const talk = sessionRef.current;
          if (!active || !talk) return;
          active.getFloatTimeDomainData(buffer);
          let squares = 0;
          for (const sample of buffer) squares += sample * sample;
          talk.observeLevel(Math.sqrt(squares / buffer.length));
        }, METER_INTERVAL_MS);
      }
      const preferred = ['audio/webm;codecs=opus', 'audio/webm'].find((type) =>
        MediaRecorder.isTypeSupported(type),
      );
      const recorder = preferred
        ? new MediaRecorder(streamRef.current, { mimeType: preferred })
        : new MediaRecorder(streamRef.current);
      chunksRef.current = [];
      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) chunksRef.current.push(event.data);
      };
      recorderRef.current = recorder;
      recorder.start(250);
    };

    const stopRecording = () =>
      new Promise<TalkRecording | null>((resolve) => {
        const recorder = recorderRef.current;
        if (!recorder || recorder.state === 'inactive') {
          resolve(null);
          return;
        }
        recorder.onstop = () => {
          const mediaType = recorder.mimeType || 'audio/webm';
          const blob = new Blob(chunksRef.current, { type: mediaType });
          chunksRef.current = [];
          recorderRef.current = null;
          resolve(blob.size > 0 ? { blob, mediaType } : null);
        };
        recorder.stop();
      });

    return {
      startRecording,
      stopRecording,
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
        const session = useSessionStore
          .getState()
          .sessions.find((entry) => entry.id === sessionId);
        activeTurnRef.current = {
          turnId: utteranceId,
          fromIndex: session?.messages.length ?? 0,
          spoken: '',
        };
        try {
          await runAgentTurn(sessionId, text, [], undefined, utteranceId, [], [], false, null, 'voice');
        } finally {
          // The turn is over when the call that ran it settles — a turn that
          // only ran tools and said nothing included, which is why the
          // microphone is released here and not on the arrival of some text.
          // The store's running flag cannot tell two overlapping turns apart;
          // this can, and the engine drops the id it has moved past.
          activeTurnRef.current = null;
          sessionRef.current?.onTurnFinished(utteranceId);
        }
      },
      cancelTurn: () => stopTurn(sessionId),
      synthesize: async (text, jobId) => {
        const speech = await talkClient.synthesize(jobId, text);
        return { audioBase64: speech.audioBase64, mediaType: speech.mediaType };
      },
      play: async (audioBase64, mediaType) => {
        // Read the chosen output before every chunk rather than freezing it for
        // the session: moving to headphones mid-conversation should be audible
        // on the next sentence. `config()` is an in-memory read on the Rust
        // side, and a read that fails is not worth dropping a sentence over —
        // the device the operator last chose is still the best guess.
        try {
          outputDeviceRef.current = (await companionClient.config()).voice.outputDeviceId;
        } catch {
          /* keep the last known output */
        }
        await player.play(base64AudioBlob(audioBase64, mediaType), outputDeviceRef.current);
      },
      stopPlayback: () => player.stop(),
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
          // Wake detection only ever arms when the operator turned it on, and
          // the Rust side refuses the setting unless transcription is local.
          wakePhrase: config.voice.wakePhraseEnabled ? config.voice.wakePhrase : null,
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
      releaseDevices();
    };
    // `ports` is memoized on the session; `mode` is applied through `setMode`
    // below rather than by rebuilding the engine mid-conversation.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoStartMode, enabled, ports, releaseDevices, sessionId]);

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
      if (delta) engine.onAssistantDelta(delta, active.turnId);
    });
  }, [sessionId]);

  const start = useCallback(async () => {
    setSetupError(null);
    try {
      await sessionRef.current?.start();
    } catch (reason) {
      setSetupError(errorMessage(reason));
    }
  }, []);

  const stop = useCallback(async () => {
    await sessionRef.current?.stop();
    releaseDevices();
  }, [releaseDevices]);
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
