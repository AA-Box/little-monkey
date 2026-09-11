// @vitest-environment jsdom
/**
 * The endpoint picker, judged on whether what it shows is true.
 *
 * Three things here are worth a test because each one has a silent failure
 * mode. A daemon that is not running must cost the operator paired endpoints
 * and nothing else — an unhandled rejection there turned all of Talk into an
 * error banner. An endpoint the daemon already knows cannot work must say so
 * and refuse to be picked, rather than being offered and failing at capture
 * time. And a `<select>` holding a value that no option carries displays the
 * first option instead, so the operator reads one device and commits another.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => undefined) }));

import { VoiceRouteSelector } from './VoiceRouteSelector';
import type { AudioEndpointDescriptor, VoiceRouteRecord } from '../../lib/daemonClient';

/** A paired endpoint exactly as `voice_route_endpoints` describes one. */
function paired(overrides: Partial<AudioEndpointDescriptor> = {}): AudioEndpointDescriptor {
  return {
    id: 'paired:phone-9:input',
    label: 'Ahmad’s phone — microphone',
    direction: 'input',
    locality: 'paired',
    device_id: 'phone-9',
    input_supported: true,
    output_supported: false,
    voice_stream_supported: true,
    os_permission: 'granted',
    readiness: 'ready',
    foreground_required: false,
    interaction_required: false,
    online: true,
    last_seen_at_ms: 1,
    latency_ms: null,
    ready: true,
    blocked_code: null,
    blocked_by: null,
    ...overrides,
  };
}

function route(overrides: Partial<VoiceRouteRecord> = {}): VoiceRouteRecord {
  return {
    session_id: 'session-1',
    route_id: 'route-1',
    generation: 3,
    engine: 'pipeline',
    input_endpoint: 'local:input:mic-1',
    output_endpoint: 'local:output:speaker-1',
    state: 'active',
    input_command_id: null,
    output_command_id: null,
    created_at_ms: 1,
    updated_at_ms: 1,
    ...overrides,
  };
}

/**
 * The daemon and the config, answering per command.
 *
 * `endpoints: null` is the daemon rejecting, and `endpoints: 'null-reply'` is
 * the daemon answering with a bare null — the two shapes an absent daemon
 * actually takes in a webview, depending on whether the bridge or the command
 * is the thing that is missing.
 */
function mock(options: {
  endpoints?: AudioEndpointDescriptor[] | null | 'null-reply';
  saved?: VoiceRouteRecord | null;
  voice?: { inputDeviceId: string | null; outputDeviceId: string | null };
} = {}) {
  invoke.mockImplementation((command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'voice_route_endpoints':
        if (options.endpoints === null) return Promise.reject(new Error('daemon is not running'));
        if (options.endpoints === 'null-reply') return Promise.resolve(null);
        return Promise.resolve({ endpoints: options.endpoints ?? [] });
      case 'voice_route_get':
        return options.endpoints === null
          ? Promise.reject(new Error('daemon is not running'))
          : Promise.resolve(options.saved ?? null);
      case 'voice_route_set':
        return Promise.resolve(route({
          input_endpoint: args?.input as string,
          output_endpoint: args?.output as string,
        }));
      case 'voice_route_move':
        return Promise.resolve(route({
          generation: 4,
          input_endpoint: (args?.input as string) ?? 'local:input:mic-1',
          output_endpoint: (args?.output as string) ?? 'local:output:speaker-1',
        }));
      case 'm7_config_get':
        return Promise.resolve({
          schemaVersion: 1,
          overlayShortcut: '',
          imageEndpoints: [],
          voice: options.voice ?? { inputDeviceId: 'mic-1', outputDeviceId: 'speaker-1' },
        });
      default:
        return Promise.resolve(null);
    }
  });
}

/** The two audio devices jsdom does not have. */
function stubDevices(devices: Partial<MediaDeviceInfo>[] = [
  { deviceId: 'mic-1', kind: 'audioinput', label: 'Built-in microphone' },
  { deviceId: 'speaker-1', kind: 'audiooutput', label: 'Built-in speaker' },
]) {
  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: { enumerateDevices: async () => devices },
  });
  Object.defineProperty(navigator, 'permissions', {
    configurable: true,
    value: { query: async () => ({ state: 'granted' }) },
  });
}

const microphone = () => screen.getByRole('combobox', { name: /microphone/i }) as HTMLSelectElement;
const speaker = () => screen.getByRole('combobox', { name: /speaker/i }) as HTMLSelectElement;
const options = (select: HTMLSelectElement) => Array.from(select.options);
const command = (name: string) => invoke.mock.calls.find((call) => call[0] === name);

beforeEach(() => {
  invoke.mockReset();
  stubDevices();
});

afterEach(() => {
  cleanup();
});

describe('VoiceRouteSelector', () => {
  it('still offers the local microphones and speakers when the daemon cannot be reached', async () => {
    // The four refresh calls used to run without a catch on the daemon's
    // endpoint list, so `remote.endpoints` threw on every webview with no
    // daemon running and Talk rendered "Cannot read properties of null".
    mock({ endpoints: null });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() =>
      expect(options(microphone()).map((option) => option.textContent)).toContain('Built-in microphone'),
    );
    expect(options(speaker()).map((option) => option.textContent)).toContain('Built-in speaker');
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('still offers the local microphones when the daemon answers with a bare null', async () => {
    // A missing command answers `null` rather than rejecting, so catching the
    // rejection alone still left `remote.endpoints` reading through null — the
    // exact banner Talk showed in a webview with no daemon behind it.
    mock({ endpoints: 'null-reply' });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() =>
      expect(options(microphone()).map((option) => option.textContent)).toContain('Built-in microphone'),
    );
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('falls back to a generic local default when the browser enumerates nothing either', async () => {
    // With no daemon *and* no enumerable devices there is still a machine with
    // a microphone on it; an empty picker would be a dead end.
    mock({ endpoints: null, voice: { inputDeviceId: null, outputDeviceId: null } });
    stubDevices([]);
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() => expect(options(microphone())).toHaveLength(1));
    expect(microphone().value).toBe('local:input:default');
    expect(speaker().value).toBe('local:output:default');
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('refuses a paired endpoint the daemon already blocked, and names the fix', async () => {
    mock({
      endpoints: [paired({
        ready: false,
        readiness: 'foreground_required',
        foreground_required: true,
        blocked_code: 'foreground_required',
        blocked_by: "'voice stream' needs the paired-device controller open and in front on the device.",
      })],
    });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    const option = await screen.findByRole('option', { name: /Ahmad’s phone/ }) as HTMLOptionElement;
    // Offering it without saying why would fail at capture time instead.
    expect(option.disabled).toBe(true);
    expect(option.textContent).toContain('bring device to foreground');
  });

  it('states the reason for every blocked code the daemon can return', async () => {
    mock({
      endpoints: [
        paired({ id: 'paired:a:input', label: 'A', blocked_code: 'permission_required', ready: false }),
        paired({ id: 'paired:b:input', label: 'B', blocked_code: 'interaction_required', ready: false }),
        paired({ id: 'paired:c:input', label: 'C', blocked_code: 'not_granted', ready: false }),
        paired({ id: 'paired:d:input', label: 'D', blocked_code: 'offline', ready: false, online: false }),
      ],
    });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() => expect(options(microphone()).length).toBeGreaterThan(4));
    const said = Object.fromEntries(
      options(microphone()).map((option) => [option.value, option.textContent ?? '']),
    );
    expect(said['paired:a:input']).toContain('needs microphone permission');
    expect(said['paired:b:input']).toContain('tap device to enable audio');
    expect(said['paired:c:input']).toContain('capability not granted');
    expect(said['paired:d:input']).toContain('offline');
    // None of these is usable, so none of them may be chosen.
    for (const id of ['paired:a:input', 'paired:b:input', 'paired:c:input', 'paired:d:input']) {
      expect((options(microphone()).find((option) => option.value === id))!.disabled).toBe(true);
    }
  });

  it('opens a route on the daemon with both endpoints and this engine when none exists yet', async () => {
    mock({ endpoints: [paired()] });
    const onRoute = vi.fn();
    render(<VoiceRouteSelector sessionId="session-1" engine="realtime" onRoute={onRoute} />);

    await waitFor(() => expect(options(microphone()).length).toBeGreaterThan(1));
    fireEvent.change(microphone(), { target: { value: 'paired:phone-9:input' } });

    await waitFor(() => expect(command('voice_route_set')).toBeTruthy());
    // The speaker is carried along: a route names both ends, and the engine is
    // the host's to record, not the device's.
    expect(command('voice_route_set')![1]).toEqual({
      sessionId: 'session-1',
      input: 'paired:phone-9:input',
      output: 'local:output:speaker-1',
      engine: 'realtime',
    });
    await waitFor(() => expect(onRoute).toHaveBeenCalled());
  });

  it('moves only the end that changed when a route is already live', async () => {
    mock({ endpoints: [paired()], saved: route() });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() => expect(options(microphone()).length).toBeGreaterThan(1));
    fireEvent.change(microphone(), { target: { value: 'paired:phone-9:input' } });

    await waitFor(() => expect(command('voice_route_move')).toBeTruthy());
    // Naming the unchanged output too would bump its generation and tear down a
    // speaker nobody asked to change.
    expect(command('voice_route_move')![1]).toEqual({
      sessionId: 'session-1',
      input: 'paired:phone-9:input',
      output: null,
    });
    expect(command('voice_route_set')).toBeUndefined();
  });

  it('never displays a selection that no option carries', async () => {
    // The phone this session is routed to has since gone away. A select whose
    // value is absent from its options shows the first option while holding the
    // other id, so the operator would read "Built-in microphone" and then move
    // a route they never looked at.
    mock({ endpoints: [], saved: route({ input_endpoint: 'paired:phone-9:input' }) });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() => expect(microphone().value).toBe('paired:phone-9:input'));
    const shown = options(microphone()).find((option) => option.value === microphone().value)!;
    expect(shown.textContent).toContain('unavailable');
    expect(shown.disabled).toBe(true);
    // And the claim in general: whatever either select holds, it is on offer.
    for (const select of [microphone(), speaker()]) {
      expect(options(select).map((option) => option.value)).toContain(select.value);
    }
  });

  it('never displays a saved default device that is no longer enumerable', async () => {
    // A config remembering a USB microphone that was unplugged is the same
    // defect without any daemon involved.
    mock({ endpoints: null, voice: { inputDeviceId: 'mic-gone', outputDeviceId: 'speaker-1' } });
    render(<VoiceRouteSelector sessionId="session-1" engine="pipeline" />);

    await waitFor(() => expect(microphone().value).toBe('local:input:mic-gone'));
    expect(options(microphone()).map((option) => option.value)).toContain('local:input:mic-gone');
    expect(
      options(microphone()).find((option) => option.value === 'local:input:mic-gone')!.textContent,
    ).toContain('unavailable');
  });
});
