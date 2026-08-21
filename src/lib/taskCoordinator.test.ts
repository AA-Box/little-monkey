import { describe, expect, it } from 'vitest';
import { coordinateToolInvocation } from './taskCoordinator';

describe('universal task coordinator routing', () => {
  it('routes native Computer Use through observe/authorize/verify phases', () => {
    expect(coordinateToolInvocation('computer_click', { target_application_id: 'Notes' })).toEqual({
      route: 'native',
      phases: ['observe', 'decide', 'authorize', 'execute', 'verify'],
      maxAttempts: 1,
    });
  });

  it('refuses native routing for browser URLs', () => {
    const decision = coordinateToolInvocation('computer_click', {
      target_application_id: 'https://example.test',
    });
    expect(decision.error).toMatch(/browser tools/i);
    expect(decision.maxAttempts).toBe(0);
  });

  it('keeps shell and browser routing distinct', () => {
    expect(coordinateToolInvocation('run_shell', {}).route).toBe('shell');
    expect(coordinateToolInvocation('web_fetch', {}).route).toBe('browser');
  });
});
