// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  PEER_METRICS_KEY, chooseSwarmPeer, loadPeerObservations, recordPeerObservation,
  type PeerPlacementCandidate,
} from './swarmPeerMetrics';

const candidates: PeerPlacementCandidate[] = [
  { alias: 'node-a', grants: ['task'], presence: 'online' },
  { alias: 'node-b', grants: ['task'], presence: 'online' },
];

beforeEach(() => {
  const memory = new Map<string, string>();
  // The shared Vitest runner also executes non-DOM tests. Install the
  // deterministic storage boundary instead of assuming jsdom exposes one.
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => memory.get(key) ?? null,
    setItem: (key: string, value: string) => { memory.set(key, value); },
    removeItem: (key: string) => { memory.delete(key); },
  });
});

describe('opt-in adaptive peer placement', () => {
  it('only selects a peer from the explicit approved pool, respecting grants and health', () => {
    expect(chooseSwarmPeer(['node-a'], candidates, {})).toBe('node-a');
    expect(chooseSwarmPeer(['node-c'], candidates, {})).toBeNull();
    expect(chooseSwarmPeer(['node-a'], [{ ...candidates[0], grants: ['message'] }], {})).toBeNull();
    expect(chooseSwarmPeer(['node-a'], [{ ...candidates[0], presence: 'offline' }], {})).toBeNull();
    expect(chooseSwarmPeer(['node-a', 'node-b'], [
      { ...candidates[0], presence: 'unknown' }, candidates[1],
    ], {})).toBe('node-b');
  });

  it('observes completed tasks, selects empirically reliable peers after enough evidence', () => {
    for (let i = 0; i < 4; i++) {
      recordPeerObservation('node-a', 'error', 1100);
      recordPeerObservation('node-b', 'done', 400);
    }
    const observed = loadPeerObservations();
    expect(observed['node-a'].failures).toBe(4);
    expect(observed['node-b'].successes).toBe(4);
    expect(chooseSwarmPeer(['node-a', 'node-b'], candidates, observed)).toBe('node-b');
  });

  it('prefers the user-provided order with insufficient evidence', () => {
    recordPeerObservation('node-b', 'done', 100);
    expect(chooseSwarmPeer(['node-a', 'node-b'], candidates, loadPeerObservations())).toBe('node-a');
  });

  it('does not store sensitive prompts or treat local cancellation as peer failure', () => {
    recordPeerObservation('node-a', 'cancelled', 40);
    expect(loadPeerObservations()).toEqual({});
    recordPeerObservation('node-a', 'done', 300);
    expect(localStorage.getItem(PEER_METRICS_KEY)).not.toContain('prompt');
    localStorage.setItem(PEER_METRICS_KEY, JSON.stringify({ 'node-a': { successes: -1, failures: 0, totalSuccessMs: 0 } }));
    expect(loadPeerObservations()).toEqual({});
  });
});
