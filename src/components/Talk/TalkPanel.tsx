/**
 * Talk: the conversation surface.
 *
 * This file is the browser half — a microphone, a recorder, a level meter and a
 * speaker — wired to `talkEngine.ts`, which owns every decision. Nothing here
 * decides when an utterance ended or what may be spoken; it opens devices,
 * moves bytes, and draws what the engine reports.
 *
 * The turn itself is an ordinary one. `runAgentTurn(..., 'voice')` is the same
 * call the composer's Send makes, into the same session, with the same model
 * routing, tools, memory and approvals — so a spoken question and a typed one
 * land in one transcript and the operator can switch between them mid-thought.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Loader2, Mic, MicOff, Radio, Square, Type, Volume2, X } from 'lucide-react';

import { runAgentTurn, stopTurn } from '../../lib/agentLoop';
import { blobToBase64, companionClient, type CaptureGrant } from '../../lib/companionClient';
import { errorMessage } from '../../lib/errors';
import { base64AudioBlob } from '../../lib/talkAudio';
import { talkClient, type TalkStatus } from '../../lib/talkClient';
import {
  TalkSession,
  type TalkMode,
  type TalkPorts,
  type TalkRecording,
  type TalkSnapshot,
  type TalkState,
} from '../../lib/talkEngine';
import { useSessionStore } from '../../store/sessionStore';
import { Button, IconButton } from '../ui';

/** Long enough for a conversation, short enough that a forgotten tab expires. */
const GRANT_LIFETIME_MS = 30 * 60_000;
/** How often the level meter samples the microphone. Matches the VAD's frame. */
const METER_INTERVAL_MS = 20;

const STATE_LABEL: Record<TalkState, string> = {
  idle: 'Not listening',
  starting: 'Starting…',
  listening: 'Listening',
  transcribing: 'Transcribing',
  thinking: 'Thinking',
  speaking: 'Speaking',
  interrupted: 'Interrupted',
  error: 'Something went wrong',
};

const STATE_TONE: Record<TalkState, string> = {
  idle: 'bg-muted',
  starting: 'bg-accent animate-pulse',
  listening: 'bg-success animate-pulse',
  transcribing: 'bg-accent animate-pulse',
  thinking: 'bg-accent animate-pulse',
  speaking: 'bg-accent',
  interrupted: 'bg-warning',
  error: 'bg-danger',
};

export interface TalkPanelProps {
  sessionId: string;
  onClose: () => void;
  /** Leave Talk and put the caret back in the composer. */
  onReturnToChat?: () => void;
  onOpenVoiceSettings?: () => void;
}

export function TalkPanel({
  sessionId,
  onClose,
  onReturnToChat,
  onOpenVoiceSettings,
}: TalkPanelProps) {
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
  const meterRef = useRef<number | null>(null);
  const playerRef = useRef<HTMLAudioElement | null>(null);
  const grantRef = useRef<CaptureGrant | null>(null);
  const deltaCursorRef = useRef(0);
  /** True once the store has confirmed this session's turn is running, so the
   * end of a turn is an observed transition rather than an assumption. */
  const turnWasRunningRef = useRef(false);

  useEffect(() => {
    grantRef.current = grant;
  }, [grant]);

  useEffect(() => {
    void talkClient
      .status()
      .then(setStatus)
      .catch((reason) => setSetupError(errorMessage(reason)));
  }, []);

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
        const analyser = context.createAnalyser();
        analyser.fftSize = 1024;
        context.createMediaStreamSource(streamRef.current).connect(analyser);
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
        const result = await companionClient.transcribeAudio(
          active.grantId,
          jobId,
          audioBase64,
          recording.mediaType,
        );
        return result.text;
      },
      submitTurn: async (text, utteranceId) => {
        // The composer's own call. `voice` only labels where the turn was made.
        deltaCursorRef.current = 0;
        await runAgentTurn(sessionId, text, [], undefined, utteranceId, [], [], false, null, 'voice');
      },
      cancelTurn: () => stopTurn(sessionId),
      synthesize: async (text, jobId) => {
        const speech = await talkClient.synthesize(jobId, text);
        return { audioBase64: speech.audioBase64, mediaType: speech.mediaType };
      },
      play: (audioBase64, mediaType) =>
        new Promise<void>((resolve) => {
          const url = URL.createObjectURL(base64AudioBlob(audioBase64, mediaType));
          const player = new Audio(url);
          playerRef.current = player;
          const finish = () => {
            URL.revokeObjectURL(url);
            if (playerRef.current === player) playerRef.current = null;
            resolve();
          };
          player.onended = finish;
          // A speaker that refuses to play must not stall the queue behind it.
          player.onerror = finish;
          void player.play().catch(finish);
        }),
      stopPlayback: () => {
        const player = playerRef.current;
        if (!player) return;
        player.pause();
        player.currentTime = 0;
        playerRef.current = null;
      },
      recordMetric: (metric) => {
        void talkClient.recordMetric(metric).catch(() => undefined);
      },
    };
  }, [ensureGrant, sessionId]);

  // One engine per session. Rebuilt when the session changes, because a Talk
  // session belongs to exactly one conversation.
  useEffect(() => {
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
      })
      .catch((reason) => {
        if (!disposed) setSetupError(errorMessage(reason));
      });
    return () => {
      disposed = true;
      void engine?.stop();
      sessionRef.current = null;
      releaseDevices();
    };
    // `ports` is memoized on the session; `mode` is applied through `setMode`
    // below rather than by rebuilding the engine mid-conversation.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ports, releaseDevices, sessionId]);

  useEffect(() => {
    sessionRef.current?.setMode(mode);
  }, [mode]);

  /**
   * The assistant's answer, as the ordinary session produces it.
   *
   * Read from the session store rather than from a Talk-specific stream: the
   * spoken turn and a typed one are the same turn, and there is only one
   * transcript. The cursor is what turns "the message got longer" into the
   * delta the speech chunker needs.
   */
  useEffect(() => {
    return useSessionStore.subscribe((store) => {
      const engine = sessionRef.current;
      if (!engine) return;
      const session = store.sessions.find((entry) => entry.id === sessionId);
      if (!session) return;
      const last = session.messages[session.messages.length - 1];
      const running = store.runningTurns[sessionId] === true;
      if (last && last.role === 'assistant') {
        const text = typeof last.content === 'string' ? last.content : '';
        if (text.length > deltaCursorRef.current) {
          engine.onAssistantDelta(text.slice(deltaCursorRef.current));
          deltaCursorRef.current = text.length;
        }
      }
      // The turn is over when the loop says so, whether or not it produced
      // text — a turn that only ran tools still has to release the microphone.
      if (!running && deltaCursorRef.current >= 0 && turnWasRunningRef.current) {
        turnWasRunningRef.current = false;
        deltaCursorRef.current = 0;
        engine.onTurnFinished();
      }
      if (running) turnWasRunningRef.current = true;
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

  const state = snapshot?.state ?? 'idle';
  const running = state !== 'idle';
  const answering = state === 'thinking' || state === 'speaking';

  return (
    <section className="flex h-full min-h-0 flex-col bg-background" aria-label="Talk">
      <header className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-3">
        <span className={`h-2.5 w-2.5 shrink-0 rounded-full ${STATE_TONE[state]}`} aria-hidden />
        <div className="min-w-0">
          <h2 className="text-sm font-semibold">Talk</h2>
          <p role="status" aria-live="polite" className="truncate text-xs text-muted">
            {STATE_LABEL[state]}
            {snapshot?.awaitingWakePhrase && state === 'listening' ? ' — waiting for the wake phrase' : ''}
          </p>
        </div>
        <div className="ml-auto flex items-center gap-2">
          {onReturnToChat && (
            <Button size="sm" variant="secondary" onClick={onReturnToChat}>
              <Type size={14} />
              Back to typing
            </Button>
          )}
          <IconButton size="sm" aria-label="Close Talk" onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </header>

      {status && !status.configured && (
        <p
          role="alert"
          className="flex items-start gap-2 border-b border-warning/40 bg-warning/10 px-4 py-2 text-xs"
        >
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          <span>
            No transcription backend is configured, so nothing said here can be understood.{' '}
            {onOpenVoiceSettings && (
              <button type="button" className="underline" onClick={onOpenVoiceSettings}>
                Open voice settings
              </button>
            )}
          </span>
        </p>
      )}

      {status?.alwaysListening && (
        <p
          role="status"
          className="flex items-center gap-2 border-b border-danger/40 bg-danger/10 px-4 py-2 text-xs font-medium text-danger"
        >
          <Radio size={14} className="shrink-0 animate-pulse" />
          Always-listening is on: this machine is listening for the wake phrase whenever Talk is open.
          <Button
            className="ml-auto"
            size="sm"
            variant="danger"
            onClick={() => {
              void stop();
              void companionClient
                .config()
                .then((config) =>
                  companionClient.saveConfig({
                    ...config,
                    voice: { ...config.voice, alwaysListening: false, wakePhraseEnabled: false },
                  }),
                )
                .then(() => talkClient.status())
                .then(setStatus)
                .catch((reason) => setSetupError(errorMessage(reason)));
            }}
          >
            Stop listening
          </Button>
        </p>
      )}

      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-4">
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs font-medium text-muted">What you said</p>
          <p className="mt-1 min-h-6 text-sm">
            {snapshot?.transcript || <span className="text-faint">Nothing yet.</span>}
          </p>
        </div>
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="flex items-center gap-2 text-xs font-medium text-muted">
            Answer
            {state === 'speaking' && <Volume2 size={12} aria-label="Speaking" />}
            {state === 'thinking' && <Loader2 size={12} className="animate-spin" />}
          </p>
          <p className="mt-1 min-h-6 whitespace-pre-wrap text-sm">
            {snapshot?.assistantText || <span className="text-faint">Nothing yet.</span>}
          </p>
        </div>
        {(snapshot?.error || setupError) && (
          <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs">
            <p className="text-danger">{snapshot?.error ?? setupError}</p>
            <Button className="mt-2" size="sm" variant="secondary" onClick={() => void start()}>
              Try again
            </Button>
          </div>
        )}
      </div>

      <footer className="shrink-0 border-t border-border bg-surface p-4">
        <div
          className="mb-3 h-1.5 overflow-hidden rounded-full bg-border"
          role="meter"
          aria-label="Microphone level"
          aria-valuenow={Math.round((snapshot?.inputLevel ?? 0) * 100)}
          aria-valuemin={0}
          aria-valuemax={100}
        >
          <div
            className="h-full bg-success transition-[width] duration-75"
            style={{ width: `${Math.min(100, Math.round((snapshot?.inputLevel ?? 0) * 100))}%` }}
          />
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {!running ? (
            <Button variant="primary" onClick={() => void start()} disabled={status?.configured === false}>
              <Mic size={15} />
              Start Talk
            </Button>
          ) : (
            <Button variant="secondary" onClick={() => void stop()}>
              <MicOff size={15} />
              End Talk
            </Button>
          )}

          {mode === 'push_to_talk' && (
            <Button
              variant={snapshot?.capturing ? 'danger' : 'secondary'}
              disabled={!running}
              onPointerDown={() => void sessionRef.current?.press()}
              onPointerUp={() => void sessionRef.current?.release()}
              onPointerCancel={() => void sessionRef.current?.release()}
              onKeyDown={(event) => {
                if ((event.key === ' ' || event.key === 'Enter') && !event.repeat) {
                  event.preventDefault();
                  void sessionRef.current?.press();
                }
              }}
              onKeyUp={(event) => {
                if (event.key === ' ' || event.key === 'Enter') void sessionRef.current?.release();
              }}
            >
              <Mic size={15} />
              {snapshot?.capturing ? 'Release to send' : 'Hold to talk'}
            </Button>
          )}

          <Button
            variant="danger"
            disabled={!answering}
            onClick={() => sessionRef.current?.interrupt('stop_button')}
          >
            <Square size={14} />
            Stop
          </Button>

          <label className="ml-auto flex items-center gap-2 text-xs text-muted">
            <input
              type="checkbox"
              checked={mode === 'continuous'}
              onChange={(event) => setMode(event.target.checked ? 'continuous' : 'push_to_talk')}
            />
            Continuous — send each time I stop speaking
          </label>
        </div>
      </footer>
    </section>
  );
}
