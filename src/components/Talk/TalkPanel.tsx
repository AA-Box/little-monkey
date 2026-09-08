/**
 * Talk: the conversation surface.
 *
 * The full-surface view of a conversation `useTalkSession` runs: a level meter,
 * what was heard, what was answered, and the controls for both modes. The
 * devices and the engine live in that hook, which the chat composer's Talk
 * button drives too — this file only draws what it reports.
 */

import { AlertTriangle, Loader2, Mic, MicOff, Radio, Square, Type, Volume2, X } from 'lucide-react';
import { useEffect, useState } from 'react';

import { companionClient, type VoiceConfig } from '../../lib/companionClient';
import { errorMessage } from '../../lib/errors';
import { talkClient } from '../../lib/talkClient';
import { type TalkState } from '../../lib/talkEngine';
import { Button, IconButton } from '../ui';
import { useTalkSession } from './useTalkSession';
import { RealtimeTalkPanel } from './RealtimeTalkPanel';

const STATE_LABEL: Record<TalkState, string> = {
  off: 'Not listening',
  starting: 'Starting…',
  armed: 'Ready',
  wake_detected: 'Wake word detected',
  capturing_command: 'Listening',
  transcribing: 'Transcribing',
  thinking: 'Thinking',
  speaking: 'Speaking',
  interrupted: 'Interrupted',
  rearming: 'Rearming wake word…',
  error: 'Something went wrong',
};

const STATE_TONE: Record<TalkState, string> = {
  off: 'bg-muted',
  starting: 'bg-accent animate-pulse',
  armed: 'bg-success animate-pulse',
  wake_detected: 'bg-success',
  capturing_command: 'bg-success animate-pulse',
  transcribing: 'bg-accent animate-pulse',
  thinking: 'bg-accent animate-pulse',
  speaking: 'bg-accent',
  interrupted: 'bg-warning',
  rearming: 'bg-accent animate-pulse',
  error: 'bg-danger',
};

export interface TalkPanelProps {
  sessionId: string;
  onClose: () => void;
  /** Leave Talk and put the caret back in the composer. */
  onReturnToChat?: () => void;
  onOpenVoiceSettings?: () => void;
}

function PipelineTalkPanel({
  sessionId,
  onClose,
  onReturnToChat,
  onOpenVoiceSettings,
}: TalkPanelProps) {
  const {
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
  } = useTalkSession(sessionId);

  const state = snapshot?.state ?? 'off';
  const running = state !== 'off';
  const answering = state === 'thinking' || state === 'speaking';

  return (
    <section className="flex h-full min-h-0 flex-col bg-background" aria-label="Talk">
      <header className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-3">
        <span className={`h-2.5 w-2.5 shrink-0 rounded-full ${STATE_TONE[state]}`} aria-hidden />
        <div className="min-w-0">
          <h2 className="text-sm font-semibold">Talk</h2>
          <p role="status" aria-live="polite" className="truncate text-xs text-muted">
            {STATE_LABEL[state]}
            {snapshot?.awaitingWakeWord && state === 'armed' ? ' — listening for the wake word' : ''}
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
          {state === 'armed'
            ? 'Always listening is on: the microphone is active, local keyword spotting is armed, and full transcription starts only after the wake word.'
            : 'Always listening is on, but Talk is not claiming wake readiness until the microphone and local keyword spotter are armed.'}
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

/** Chooses the explicitly configured desktop engine. Missing legacy config
 * fields resolve to `pipeline`, preserving the existing behavior exactly. */
export function TalkPanel(props: TalkPanelProps) {
  const [voice, setVoice] = useState<VoiceConfig | null | undefined>(undefined);
  useEffect(() => {
    let active = true;
    void companionClient.config()
      .then((config) => { if (active) setVoice(config.voice); })
      .catch(() => { if (active) setVoice(null); });
    return () => { active = false; };
  }, []);
  if (voice === undefined) {
    return <section className="flex h-full items-center justify-center bg-background" aria-label="Talk"><Loader2 className="animate-spin" aria-label="Loading Talk settings" /></section>;
  }
  if (voice === null) {
    return (
      <section className="flex h-full flex-col items-center justify-center gap-3 bg-background p-6 text-center" aria-label="Talk">
        <p role="alert" className="text-sm text-danger">Talk settings could not be loaded. No voice engine was started.</p>
        {props.onOpenVoiceSettings && <Button variant="secondary" onClick={props.onOpenVoiceSettings}>Open voice settings</Button>}
      </section>
    );
  }
  if ((voice.engineKind ?? 'pipeline') === 'realtime') {
    return <RealtimeTalkPanel {...props} voice={voice} />;
  }
  return <PipelineTalkPanel {...props} />;
}
