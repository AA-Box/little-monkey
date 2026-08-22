import { describe, expect, it } from 'vitest';
import { coordinateToolInvocation, runCoordinatedInvocation } from './taskCoordinator';

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

  it('refuses native control of known browser application windows', () => {
    const decision = coordinateToolInvocation('computer_inspect', {
      target_application_id: 'com.google.Chrome',
    });
    expect(decision.error).toMatch(/browser tools/i);
    expect(decision.maxAttempts).toBe(0);
  });

  it('keeps shell and browser routing distinct', () => {
    expect(coordinateToolInvocation('run_shell', {}).route).toBe('shell');
    expect(coordinateToolInvocation('web_fetch', {}).route).toBe('browser');
  });

  it('owns phase order and stops retrying at the declared budget', async () => {
    const phases: string[] = [];
    let executions = 0;
    const result = await runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 1 },
      {
        onPhase: (phase) => { phases.push(phase); },
        execute: () => { executions += 1; return 'outcome'; },
        verify: () => false,
      },
    );
    expect(result).toBe('outcome');
    expect(executions).toBe(1);
    expect(phases).toEqual(['observe', 'decide', 'authorize', 'verify']);
  });
});
