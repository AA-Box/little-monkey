import { Loader2, RefreshCw } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';

import { companionClient, type VoiceConfig } from '../../lib/companionClient';
import {
  type AudioEndpointDescriptor,
  type VoiceRouteEngine,
  type VoiceRouteRecord,
  voiceRouteDeactivate,
  voiceRouteEndpoints,
  voiceRouteGet,
  voiceRouteMove,
  voiceRouteSet,
} from '../../lib/daemonClient';
import { errorMessage } from '../../lib/errors';
import { IconButton } from '../ui';

const localId = (direction: 'input' | 'output', id: string | null | undefined) =>
  `local:${direction}:${id || 'default'}`;

async function localMicrophonePermission(): Promise<AudioEndpointDescriptor['os_permission']> {
  if (!navigator.permissions?.query) return 'undetermined';
  try {
    // `microphone` is implemented by Chromium/WebKit even though older DOM
    // typings do not include it in PermissionName. The runtime result is the
    // authority; an unsupported query remains fail-honest as undetermined.
    const status = await navigator.permissions.query(
      { name: 'microphone' } as Parameters<Permissions['query']>[0],
    );
    if (status.state === 'granted') return 'granted';
    if (status.state === 'denied') return 'denied';
    return 'promptable';
  } catch {
    return 'undetermined';
  }
}

async function localEndpoints(): Promise<AudioEndpointDescriptor[]> {
  if (!navigator.mediaDevices?.enumerateDevices) return [];
  const [devices, microphonePermission] = await Promise.all([
    navigator.mediaDevices.enumerateDevices(),
    localMicrophonePermission(),
  ]);
  return devices
    .filter((device) => device.kind === 'audioinput' || device.kind === 'audiooutput')
    .map((device, index) => {
      const input = device.kind === 'audioinput';
      const permission = input ? microphonePermission : 'not_required';
      const permissionBlocked = input && permission !== 'granted';
      const denied = permission === 'denied';
      return {
        id: localId(input ? 'input' : 'output', device.deviceId),
        label: device.label || `${input ? 'Microphone' : 'Speaker'} ${index + 1}`,
        direction: input ? 'input' : 'output',
        locality: 'local',
        device_id: null,
        input_supported: input,
        output_supported: !input,
        voice_stream_supported: input,
        os_permission: permission,
        readiness: denied ? 'unavailable' : permissionBlocked ? 'interaction_required' : 'ready',
        foreground_required: false,
        interaction_required: permissionBlocked && !denied,
        online: true,
        last_seen_at_ms: null,
        latency_ms: null,
        ready: !permissionBlocked,
        blocked_code: permissionBlocked ? (denied ? 'permission_denied' : 'permission_required') : null,
        blocked_by: permissionBlocked
          ? denied
            ? 'Microphone permission denied'
            : 'Microphone permission will be requested when Talk starts'
          : null,
      } satisfies AudioEndpointDescriptor;
    });
}

function endpointSelectable(endpoint: AudioEndpointDescriptor): boolean {
  if (endpoint.ready) return true;
  // A local desktop microphone may be selected while permission is promptable:
  // Talk opens it from the user's start gesture. A paired device cannot be
  // prompted by the host and therefore remains disabled until it advertises
  // effective readiness itself.
  return endpoint.locality === 'local' && endpoint.blocked_code === 'permission_required';
}

function defaultRoute(voice: VoiceConfig): Pick<VoiceRouteRecord, 'input_endpoint' | 'output_endpoint'> {
  return {
    input_endpoint: localId('input', voice.inputDeviceId),
    output_endpoint: localId('output', voice.outputDeviceId),
  };
}

export function VoiceRouteSelector({
  sessionId,
  engine,
  disabled = false,
  onRoute,
}: {
  sessionId: string;
  engine: VoiceRouteEngine;
  disabled?: boolean;
  onRoute?: (route: VoiceRouteRecord) => void;
}) {
  const [endpoints, setEndpoints] = useState<AudioEndpointDescriptor[]>([]);
  const [route, setRoute] = useState<VoiceRouteRecord | null>(null);
  const [fallback, setFallback] = useState<Pick<VoiceRouteRecord, 'input_endpoint' | 'output_endpoint'> | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const [remote, local, saved, config] = await Promise.all([
        voiceRouteEndpoints(),
        localEndpoints().catch(() => []),
        voiceRouteGet(sessionId).catch(() => null),
        companionClient.config(),
      ]);
      // The daemon advertises generic local defaults for CLI users. The webview
      // knows the actual MediaDeviceInfo ids, so prefer those here and keep the
      // generic default only when the browser cannot enumerate that direction.
      const merged = [...local];
      for (const endpoint of remote.endpoints) {
        if (endpoint.locality === 'paired') merged.push(endpoint);
      }
      if (!merged.some((endpoint) => endpoint.direction === 'input')) {
        merged.push(remote.endpoints.find((endpoint) => endpoint.id === 'local:input:default')!);
      }
      if (!merged.some((endpoint) => endpoint.direction === 'output')) {
        merged.push(remote.endpoints.find((endpoint) => endpoint.id === 'local:output:default')!);
      }
      setEndpoints(merged.filter(Boolean));
      setFallback(defaultRoute(config.voice));
      let matching = saved?.state === 'active' && saved.engine === engine ? saved : null;
      // Switching Talk engines is also a capture-boundary change. Retire an
      // active route from the other engine before exposing this engine's route.
      if (saved?.state === 'active' && saved.engine !== engine
          && (saved.input_command_id || saved.output_command_id)) {
        await voiceRouteDeactivate(sessionId);
        matching = null;
      }
      setRoute(matching);
      if (matching) onRoute?.(matching);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }, [engine, onRoute, sessionId]);

  useEffect(() => {
    void refresh();
    const listener = () => void refresh();
    navigator.mediaDevices?.addEventListener?.('devicechange', listener);
    return () => navigator.mediaDevices?.removeEventListener?.('devicechange', listener);
  }, [refresh]);

  const input = route?.input_endpoint ?? fallback?.input_endpoint ?? 'local:input:default';
  const output = route?.output_endpoint ?? fallback?.output_endpoint ?? 'local:output:default';
  const inputs = useMemo(() => endpoints.filter((endpoint) => endpoint.direction === 'input'), [endpoints]);
  const outputs = useMemo(() => endpoints.filter((endpoint) => endpoint.direction === 'output'), [endpoints]);

  const apply = async (nextInput: string, nextOutput: string) => {
    setBusy(true);
    setError(null);
    try {
      const next = route
        ? await voiceRouteMove(
            sessionId,
            nextInput === input ? undefined : nextInput,
            nextOutput === output ? undefined : nextOutput,
          )
        : await voiceRouteSet(sessionId, nextInput, nextOutput, engine);
      setRoute(next);
      onRoute?.(next);
    } catch (reason) {
      setError(errorMessage(reason));
      // A failed live move may have rolled back under a newer generation. Read
      // the daemon's authoritative selection before rendering another choice.
      const restored = await voiceRouteGet(sessionId).catch(() => null);
      if (restored?.state === 'active') {
        setRoute(restored);
        onRoute?.(restored);
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-end gap-2 rounded-md border border-border bg-background p-2">
      <label className="min-w-48 flex-1 text-[11px] text-muted">
        Microphone
        <select
          className="mt-1 w-full rounded border border-border bg-surface px-2 py-1.5 text-xs text-foreground"
          value={input}
          disabled={disabled || busy}
          onChange={(event) => void apply(event.target.value, output)}
        >
          {inputs.map((endpoint) => (
            <option key={endpoint.id} value={endpoint.id} disabled={!endpointSelectable(endpoint)}>
              {endpoint.label}{endpoint.ready ? '' : ` — ${endpoint.blocked_by ?? 'not ready'}`}
            </option>
          ))}
        </select>
      </label>
      <label className="min-w-48 flex-1 text-[11px] text-muted">
        Speaker
        <select
          className="mt-1 w-full rounded border border-border bg-surface px-2 py-1.5 text-xs text-foreground"
          value={output}
          disabled={disabled || busy}
          onChange={(event) => void apply(input, event.target.value)}
        >
          {outputs.map((endpoint) => (
            <option key={endpoint.id} value={endpoint.id} disabled={!endpointSelectable(endpoint)}>
              {endpoint.label}{endpoint.ready ? '' : ` — ${endpoint.blocked_by ?? 'not ready'}`}
            </option>
          ))}
        </select>
      </label>
      <IconButton size="sm" aria-label="Refresh audio endpoints" disabled={busy} onClick={() => void refresh()}>
        {busy ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}
      </IconButton>
      {error && <p role="alert" className="w-full text-[11px] text-danger">{error}</p>}
    </div>
  );
}
