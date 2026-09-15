import { AlertTriangle, Loader2, Mic, MicOff, ShieldAlert, Square, Type, Volume2, X } from 'lucide-react';
import { useEffect, useState } from 'react';

import { realtimeVoiceClient, type RealtimeVoiceStatus, type VoiceConfig } from '../../lib/companionClient';
import type { VoiceRouteRecord } from '../../lib/daemonClient';
import type { RealtimeVoiceState } from '../../lib/realtimeVoice';
import { Button, IconButton } from '../ui';
import type { TalkPanelProps } from './TalkPanel';
import { useRealtimeVoiceSession } from './useRealtimeVoiceSession';
import { VoiceRouteSelector } from './VoiceRouteSelector';

const LABEL: Record<RealtimeVoiceState, string> = {
  idle: 'Not connected', connecting: 'Connecting securely…', ready: 'Ready',
  listening: 'Listening', responding: 'Responding', awaiting_approval: 'Waiting for approval',
  reconnecting: 'Reconnecting to OpenAI…', error: 'Connection failed', closed: 'Ended',
};

const TONE: Record<RealtimeVoiceState, string> = {
  idle: 'bg-muted', connecting: 'bg-accent animate-pulse', ready: 'bg-success',
  listening: 'bg-success animate-pulse', responding: 'bg-accent', awaiting_approval: 'bg-warning animate-pulse',
  reconnecting: 'bg-warning animate-pulse', error: 'bg-danger', closed: 'bg-muted',
};

export function RealtimeTalkPanel({
  sessionId,
  onClose,
  onReturnToChat,
  onOpenVoiceSettings,
  voice,
}: TalkPanelProps & { voice: VoiceConfig }) {
  const [voiceRoute, setVoiceRoute] = useState<VoiceRouteRecord | null>(null);
  const session = useRealtimeVoiceSession(sessionId, voice, voiceRoute);
  const [providerStatus, setProviderStatus] = useState<RealtimeVoiceStatus | null>(null);
  const [privacyAccepted, setPrivacyAccepted] = useState(false);
  const running = !['idle', 'closed', 'error'].includes(session.state);
  const manual = (voice.realtimeTurnDetection ?? 'semantic_vad') === 'manual';

  useEffect(() => {
    void realtimeVoiceClient.status().then(setProviderStatus).catch(() => setProviderStatus(null));
  }, []);

  return (
    <section className="flex h-full min-h-0 flex-col bg-background" aria-label="Talk — realtime voice">
      <header className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-3">
        <span className={`h-2.5 w-2.5 rounded-full ${TONE[session.state]}`} aria-hidden />
        <div className="min-w-0">
          <h2 className="text-sm font-semibold">Talk · Realtime</h2>
          <p role="status" aria-live="polite" className="truncate text-xs text-muted">{LABEL[session.state]}</p>
        </div>
        <div className="ml-auto flex items-center gap-2">
          {onReturnToChat && <Button size="sm" variant="secondary" onClick={onReturnToChat}><Type size={14} />Back to typing</Button>}
          <IconButton size="sm" aria-label="Close Talk" onClick={() => { void session.stop(); onClose(); }}><X size={15} /></IconButton>
        </div>
      </header>

      {!providerStatus?.configured && providerStatus !== null && (
        <p role="alert" className="flex items-start gap-2 border-b border-warning/40 bg-warning/10 px-4 py-2 text-xs">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          <span>An OpenAI API key is not available in the OS keychain. Realtime Talk will not fall back to another provider.{' '}
            {onOpenVoiceSettings && <button type="button" className="underline" onClick={onOpenVoiceSettings}>Open voice settings</button>}
          </span>
        </p>
      )}

      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-4">
        <VoiceRouteSelector sessionId={sessionId} engine="realtime" onRoute={setVoiceRoute} />
        {!running && (
          <label className="flex items-start gap-3 rounded-lg border border-warning/40 bg-warning/10 p-3 text-xs">
            <input className="mt-0.5" type="checkbox" checked={privacyAccepted} onChange={(event) => setPrivacyAccepted(event.target.checked)} />
            <span><span className="mb-1 flex items-center gap-1 font-semibold"><ShieldAlert size={14} />Before connecting</span>
              Audio from the selected microphone and the bounded conversation context are sent to OpenAI for this live session.
              Tool calls still pass through Little Monkey’s existing permission, sandbox, workspace, network, and MCP controls.
              Audio is not stored by Little Monkey.
            </span>
          </label>
        )}
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs font-medium text-muted">What you said</p>
          <p className="mt-1 min-h-6 text-sm">{session.inputTranscript || <span className="text-faint">Nothing yet.</span>}</p>
        </div>
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="flex items-center gap-2 text-xs font-medium text-muted">Answer
            {session.state === 'responding' && <Volume2 size={12} aria-label="Speaking" />}
            {(session.state === 'connecting' || session.state === 'reconnecting') && <Loader2 size={12} className="animate-spin" />}
          </p>
          <p className="mt-1 min-h-6 whitespace-pre-wrap text-sm">{session.outputTranscript || <span className="text-faint">Nothing yet.</span>}</p>
        </div>
        {session.awaitingApproval && (
          <p className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs">
            Little Monkey is running a tool through its normal boundary. If it needs a decision, the usual permission prompt appears.
          </p>
        )}
        {session.error && (
          <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs">
            <p className="text-danger">{session.error}</p>
            <Button className="mt-2" size="sm" variant="secondary" onClick={() => void session.start()}>Retry same provider</Button>
          </div>
        )}
      </div>

      <footer className="shrink-0 border-t border-border bg-surface p-4">
        <div className="flex flex-wrap items-center gap-2">
          {!running ? (
            <Button variant="primary" onClick={() => void session.start()} disabled={!privacyAccepted || providerStatus?.configured !== true}>
              <Mic size={15} />Start realtime Talk
            </Button>
          ) : (
            <Button variant="secondary" onClick={() => void session.stop()}><MicOff size={15} />End Talk</Button>
          )}
          {manual && (
            <Button
              variant="secondary"
              disabled={!running}
              onPointerDown={() => void session.startManualTurn()}
              onPointerUp={() => void session.finishManualTurn()}
              onPointerCancel={() => void session.finishManualTurn()}
            ><Mic size={15} />Hold to talk</Button>
          )}
          <Button variant="danger" disabled={session.state !== 'responding'} onClick={() => void session.interrupt()}>
            <Square size={14} />Stop response
          </Button>
          <span className="ml-auto text-xs text-muted">OpenAI · {voice.realtimeModel ?? 'gpt-realtime-2.1'} · {voice.realtimeVoice ?? 'marin'}</span>
        </div>
      </footer>
    </section>
  );
}
