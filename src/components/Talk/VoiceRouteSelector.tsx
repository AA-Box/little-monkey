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

/**
 * The daemon's generic local defaults, restated here.
 *
 * A webview with no daemon reachable still has microphones and speakers, so the
 * absence of the daemon must cost the user paired endpoints and nothing else.
 */
function localDefaultEndpoint(direction: 'input' | 'output'): AudioEndpointDescriptor {
  const input = direction === 'input';
  return {
    id: `local:${direction}:default`,
    label: input ? 'This computer — default microphone' : 'This computer — default speaker',
    direction,
    locality: 'local',
    device_id: null,
    input_supported: input,
    output_supported: !input,
    voice_stream_supported: input,
    os_permission: null,
    readiness: null,
    foreground_required: false,
    interaction_required: false,
    online: true,
    last_seen_at_ms: null,
    latency_ms: null,
    ready: true,
    blocked_code: null,
    blocked_by: null,
  };
}

/**
 * The routed endpoint when it is not among the ones on offer — a paired device
 * that went offline, or a saved default for a microphone that is gone.
 *
 * A `<select>` whose value matches no option displays the first option while
 * holding a different value, so the operator reads one device and commits
 * another. Naming the absent endpoint keeps the displayed selection truthful.
 */
function missingEndpoint(id: string, direction: 'input' | 'output'): AudioEndpointDescriptor {
  return {
    ...localDefaultEndpoint(direction),
    id,
    label: id,
    locality: id.startsWith('paired:') ? 'paired' : 'local',
    ready: false,
    blocked_code: 'unavailable',
    blocked_by: 'This endpoint is no longer available',
  };
}

/**
 * The short phrase for each reason the daemon computes, in the vocabulary of
 * the fix rather than of the protocol. `blocked_by` carries the daemon's full
 * sentence and stands in for anything this list does not know yet.
 */
const BLOCK_STATUS: Record<string, string> = {
  offline: 'offline',
  not_granted: 'capability not granted',
  no_surface: 'device has not said what it can do',
  unsupported: 'not supported by this device',
  foreground_required: 'bring device to foreground',
  interaction_required: 'tap device to enable audio',
  screen_capture_not_armed: 'screen capture not armed',
  unavailable: 'unavailable',
};

function endpointStatus(endpoint: AudioEndpointDescriptor): string | null {
  if (endpoint.ready) return null;
  if (!endpoint.online) return 'offline';
  const noun = endpoint.direction === 'input' ? 'microphone' : 'audio';
  if (endpoint.blocked_code === 'permission_required') return `needs ${noun} permission`;
  if (endpoint.blocked_code === 'permission_denied') return `${noun} permission denied`;
  return BLOCK_STATUS[endpoint.blocked_code ?? ''] ?? endpoint.blocked_by ?? 'not ready';
}

/** The options for one direction, always including whatever is selected. */
function offered(
  list: AudioEndpointDescriptor[],
  selected: string,
  direction: 'input' | 'output',
): AudioEndpointDescriptor[] {
  // While the first refresh is still running there is nothing to be untruthful
  // about, and an "unavailable" flash would be a lie of its own.
  if (list.length === 0 || list.some((endpoint) => endpoint.id === selected)) return list;
  return [...list, missingEndpoint(selected, direction)];
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
      // Only the local enumeration is indispensable. A daemon that is not
      // running — the ordinary case in a webview — costs the operator paired
      // endpoints and the saved default, never Talk itself. A daemon that is
      // not there answers by rejecting *or* by resolving null, and both used to
      // reach `remote.endpoints` and turn Talk into an error banner.
      const [remote, local, saved, config] = await Promise.all([
        voiceRouteEndpoints().catch(() => null),
        localEndpoints().catch(() => []),
        voiceRouteGet(sessionId).catch(() => null),
        companionClient.config().catch(() => null),
      ]);
      const advertised = remote?.endpoints ?? [];
      // The daemon advertises generic local defaults for CLI users. The webview
      // knows the actual MediaDeviceInfo ids, so prefer those here and keep the
      // generic default only when the browser cannot enumerate that direction.
      const merged = [...local];
      for (const endpoint of advertised) {
        if (endpoint.locality === 'paired') merged.push(endpoint);
      }
      for (const direction of ['input', 'output'] as const) {
        if (merged.some((endpoint) => endpoint.direction === direction)) continue;
        merged.push(
          advertised.find((endpoint) => endpoint.id === `local:${direction}:default`)
            ?? localDefaultEndpoint(direction),
        );
      }
      setEndpoints(merged);
      setFallback(config?.voice ? defaultRoute(config.voice) : null);
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
  const inputs = useMemo(
    () => offered(endpoints.filter((endpoint) => endpoint.direction === 'input'), input, 'input'),
    [endpoints, input],
  );
  const outputs = useMemo(
    () => offered(endpoints.filter((endpoint) => endpoint.direction === 'output'), output, 'output'),
    [endpoints, output],
  );

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
              {endpoint.label}{endpointStatus(endpoint) ? ` — ${endpointStatus(endpoint)}` : ''}
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
              {endpoint.label}{endpointStatus(endpoint) ? ` — ${endpointStatus(endpoint)}` : ''}
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
