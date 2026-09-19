/**
 * The realtime voice engine, inside the composer.
 *
 * Rendered *instead of* the pipeline status strip, never alongside it. That is
 * deliberate and structural: `useRealtimeVoiceSession` has no `enabled` option
 * and deactivates the voice route unconditionally when it unmounts or the
 * session changes, so two engines mounted at once would fight over one route.
 * Choosing between components rather than between hooks keeps that hook exactly
 * as it was.
 *
 * The consent gate is preserved as it stood on the page it replaced: pressing
 * the composer's microphone mounts this bar idle, showing the checkbox, and
 * nothing connects until Start is pressed here. Audio reaching OpenAI still
 * requires a deliberate second act.
 */
import { AlertTriangle, Loader2, Mic, MicOff, ShieldAlert, Square, Volume2 } from 'lucide-react';
import { useEffect, useState } from 'react';

import { realtimeVoiceClient, type RealtimeVoiceStatus, type VoiceConfig } from '../../lib/companionClient';
import type { VoiceRouteRecord } from '../../lib/daemonClient';
import type { RealtimeVoiceState } from '../../lib/realtimeVoice';
import { useT } from '../../lib/i18n';
import { Button } from '../ui';
import { useRealtimeVoiceSession } from '../Talk/useRealtimeVoiceSession';

const STATE_KEY: Record<RealtimeVoiceState, string> = {
  idle: 'RealtimeTalk.stateIdle',
  connecting: 'RealtimeTalk.stateConnecting',
  ready: 'RealtimeTalk.stateReady',
  listening: 'RealtimeTalk.stateListening',
  responding: 'RealtimeTalk.stateResponding',
  awaiting_approval: 'RealtimeTalk.stateAwaitingApproval',
  reconnecting: 'RealtimeTalk.stateReconnecting',
  error: 'RealtimeTalk.stateError',
  closed: 'RealtimeTalk.stateClosed',
};

const TONE: Record<RealtimeVoiceState, string> = {
  idle: 'bg-muted', connecting: 'bg-accent animate-pulse', ready: 'bg-success',
  listening: 'bg-success animate-pulse', responding: 'bg-accent', awaiting_approval: 'bg-warning animate-pulse',
  reconnecting: 'bg-warning animate-pulse', error: 'bg-danger', closed: 'bg-muted',
};

export function RealtimeTalkBar({
  sessionId,
  voice,
  route,
  onOpenVoiceSettings,
  onEnd,
}: {
  sessionId: string;
  voice: VoiceConfig;
  route: VoiceRouteRecord | null;
  onOpenVoiceSettings: () => void;
  onEnd: () => void;
}) {
  const { t } = useT();
  const session = useRealtimeVoiceSession(sessionId, voice, route);
  const [providerStatus, setProviderStatus] = useState<RealtimeVoiceStatus | null>(null);
  const [privacyAccepted, setPrivacyAccepted] = useState(false);
  const running = !['idle', 'closed', 'error'].includes(session.state);
  const manual = (voice.realtimeTurnDetection ?? 'semantic_vad') === 'manual';

  useEffect(() => {
    void realtimeVoiceClient.status().then(setProviderStatus).catch(() => setProviderStatus(null));
  }, []);

  return (
    <div className="flex flex-col gap-2 border-t border-border px-3 py-2 text-xs">
      <div className="flex items-center gap-2">
        <span className={`h-2 w-2 shrink-0 rounded-full ${TONE[session.state]}`} aria-hidden />
        <span role="status" aria-live="polite" className="shrink-0 text-muted">{t(STATE_KEY[session.state])}</span>
        {/* One line, truncated. A half-heard sentence is not a message, so it
            never enters the transcript — finalized text already arrives as an
            ordinary chat message. */}
        <span className="min-w-0 flex-1 truncate text-faint">{session.outputTranscript || session.inputTranscript}</span>
        {session.state === 'responding' && <Volume2 size={12} aria-label={t('RealtimeTalk.stateResponding')} />}
        {(session.state === 'connecting' || session.state === 'reconnecting') && <Loader2 size={12} className="animate-spin" />}
      </div>

      {providerStatus !== null && !providerStatus.configured && (
        <p role="alert" className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-2">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          <span>
            An OpenAI API key is not available in the OS keychain. Realtime Talk will not fall back to another provider.{' '}
            <button type="button" className="underline" onClick={onOpenVoiceSettings}>{t('TalkMenu.openVoiceSettings')}</button>
          </span>
        </p>
      )}

      {!running && (
        <label className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-2">
          <input className="mt-0.5" type="checkbox" checked={privacyAccepted} onChange={(event) => setPrivacyAccepted(event.target.checked)} />
          <span>
            <span className="mb-1 flex items-center gap-1 font-semibold"><ShieldAlert size={14} />Before connecting</span>
            Audio from the selected microphone and the bounded conversation context are sent to OpenAI for this live session.
            Tool calls still pass through Little Monkey’s existing permission, sandbox, workspace, network, and MCP controls.
            Audio is not stored by Little Monkey.
          </span>
        </label>
      )}

      {session.awaitingApproval && (
        <p className="rounded-md border border-warning/40 bg-warning/10 p-2">
          Little Monkey is running a tool through its normal boundary. If it needs a decision, the usual permission prompt appears.
        </p>
      )}

      {session.error && (
        <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2">
          <p className="text-danger">{session.error}</p>
          <Button className="mt-1.5" size="sm" variant="secondary" onClick={() => void session.start()}>Retry same provider</Button>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        {!running ? (
          <Button size="sm" variant="primary" onClick={() => void session.start()} disabled={!privacyAccepted || providerStatus?.configured !== true}>
            <Mic size={14} />Start realtime Talk
          </Button>
        ) : (
          <Button size="sm" variant="secondary" onClick={() => { void session.stop(); onEnd(); }}>
            <MicOff size={14} />{t('ChatWindow.talkStopAriaLabel')}
          </Button>
        )}
        {/* Realtime turn detection is a persisted setting, not a runtime mode,
            so this appears only when the operator chose `manual` in settings —
            the composer's continuous checkbox cannot drive it. */}
        {manual && (
          <Button
            size="sm"
            variant="secondary"
            disabled={!running}
            onPointerDown={() => void session.startManualTurn()}
            onPointerUp={() => void session.finishManualTurn()}
            onPointerCancel={() => void session.finishManualTurn()}
          >
            <Mic size={14} />{t('ChatWindow.talkHoldToTalk')}
          </Button>
        )}
        <Button size="sm" variant="danger" disabled={session.state !== 'responding'} onClick={() => void session.interrupt()}>
          <Square size={12} />Stop response
        </Button>
        <span className="ml-auto text-faint">OpenAI · {voice.realtimeModel ?? 'gpt-realtime-2.1'} · {voice.realtimeVoice ?? 'marin'}</span>
      </div>
    </div>
  );
}
