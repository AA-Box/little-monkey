// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  SPECIALIST_METRICS_KEY,
  chooseSupplementalSpecialist,
  loadSpecialistMetrics,
  recordSpecialistMetric,
} from './swarmSpecialistMetrics';

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

describe('adaptive specialist selection', () => {
  const options = ['Threat model reviewer', 'Secrets exposure reviewer', 'Race condition reviewer'];

  it('samples under-tested specialist roles before ranking their results', () => {
    expect(chooseSupplementalSpecialist('security', options)).toBe(options[0]);
    recordSpecialistMetric('security', options[0], true, 250);
    expect(chooseSupplementalSpecialist('security', options)).toBe(options[1]);
    recordSpecialistMetric('security', options[1], false, 500);
    expect(chooseSupplementalSpecialist('security', options)).toBe(options[2]);
  });

  it('uses empirical reliability after each candidate has at least three actual attempts', () => {
    for (const role of options) {
      for (let n = 0; n < 3; n++) recordSpecialistMetric('security', role, role !== options[0], 500 + n);
    }
    expect(chooseSupplementalSpecialist('security', options)).toBe(options[1]);
    expect(loadSpecialistMetrics()['security:' + options[0]].failures).toBe(3);
  });

  it('stores only bounded aggregate counts and durations, never task data', () => {
    recordSpecialistMetric('backend', 'Backend caching reviewer', true, 70);
    const raw = localStorage.getItem(SPECIALIST_METRICS_KEY) ?? '';
    expect(raw).toContain('completedMs');
    expect(raw).not.toContain('prompt');
    recordSpecialistMetric('backend', 'Backend caching reviewer', true, NaN);
    expect(loadSpecialistMetrics()['backend:Backend caching reviewer'].successes).toBe(1);
  });

  it('rejects corrupt cached metrics and empty specialist pools', () => {
    localStorage.setItem(SPECIALIST_METRICS_KEY, JSON.stringify({
      'security:Threat model reviewer': { successes: -20, failures: 0, completedMs: 1 },
    }));
    expect(loadSpecialistMetrics()).toEqual({});
    expect(() => chooseSupplementalSpecialist('security', [])).toThrow(/empty/);
  });
});
