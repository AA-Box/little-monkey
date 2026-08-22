import { describe, expect, it } from 'vitest';
import {
  ComputerUseRunBudget,
  CoordinatedInvocationError,
  CoordinatedRetryableError,
  computerUseFailure,
  coordinateToolInvocation,
  runCoordinatedInvocation,
} from './taskCoordinator';

describe('universal task coordinator routing', () => {
  it('routes native Computer Use through observe/authorize/verify phases', () => {
    expect(coordinateToolInvocation('computer_click', { target_application_id: 'Notes' })).toEqual({
      route: 'native',
      phases: ['observe', 'decide', 'authorize', 'execute', 'verify'],
      maxAttempts: 2,
    });
  });

  it('keeps browser work single-attempt while allowing one safe native recovery', () => {
    expect(coordinateToolInvocation('browser_inspect', { url: 'https://example.test' }).maxAttempts).toBe(1);
    expect(coordinateToolInvocation('computer_inspect', { target_application_id: 'Notes' }).maxAttempts).toBe(2);
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
    await expect(runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 1 },
      {
        onPhase: (phase) => { phases.push(phase); },
        execute: () => { executions += 1; return 'outcome'; },
        verify: () => false,
      },
    )).rejects.toBeInstanceOf(CoordinatedInvocationError);
    expect(executions).toBe(1);
    expect(phases).toEqual(['observe', 'decide', 'authorize', 'verify']);
  });

  it('re-runs observation and execution after a pre-input phase failure', async () => {
    const phases: string[] = [];
    let executeFailures = 0;
    let executions = 0;
    const budget = new ComputerUseRunBudget({ maxRetries: 1 });
    const result = await runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 2 },
      {
        onPhase: (phase, attempt) => {
          phases.push(`${attempt}:${phase}`);
        },
        execute: () => {
          executions += 1;
          if (executeFailures++ === 0) {
            throw new CoordinatedRetryableError(computerUseFailure('provider unavailable before input', {
              code: 'PROVIDER_TRANSIENT_PRE_INPUT',
              inputSent: false,
              safeToRetry: true,
              phase: 'pre_execute',
            }));
          }
          return 'recovered';
        },
        verify: () => true,
        budget,
      },
    );
    expect(result).toBe('recovered');
    expect(executions).toBe(2);
    expect(budget.remaining('retries')).toBe(0);
    expect(phases).toEqual([
      '1:observe', '1:decide', '1:authorize',
      '2:observe', '2:decide', '2:authorize', '2:verify',
    ]);
  });

  it('fails closed for an untyped phase exception', async () => {
    let executions = 0;
    await expect(runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 2 },
      {
        onPhase: (phase) => { if (phase === 'observe') throw new Error('unknown backend exception'); },
        execute: () => { executions += 1; return 'never'; },
      },
    )).rejects.toThrow('unknown backend exception');
    expect(executions).toBe(0);
  });

  it('never retries once the native backend reports input sent without verification', async () => {
    let executions = 0;
    await expect(runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 2 },
      {
        execute: () => { executions += 1; return 'outcome'; },
        verify: () => { throw new CoordinatedInvocationError(); },
      },
    )).rejects.toBeInstanceOf(CoordinatedInvocationError);
    expect(executions).toBe(1);
  });

  it('enforces the configured retry budget and activates model-call charging lazily', async () => {
    const budget = new ComputerUseRunBudget({ maxRetries: 0, maxActions: 1, maxModelCalls: 1 });
    expect(budget.consume('model_calls')).toBe(true);
    expect(budget.consume('model_calls')).toBe(true);
    expect(budget.consume('actions')).toBe(true);
    expect(budget.consume('model_calls')).toBe(true);
    expect(budget.consume('model_calls')).toBe(false);

    await expect(runCoordinatedInvocation(
      { route: 'native', phases: ['observe', 'decide', 'authorize', 'execute', 'verify'], maxAttempts: 2 },
      { budget, execute: () => 'outcome', verify: () => false },
    )).rejects.toThrow(/retry limit/i);
  });

  it('records a mixed browser/native golden route without allowing native browser control', async () => {
    const trace: string[] = [];
    const browser = coordinateToolInvocation('browser_inspect', { url: 'https://example.test' });
    const native = coordinateToolInvocation('computer_inspect', { target_application_id: 'Notes' });
    await runCoordinatedInvocation(browser, {
      onPhase: (phase) => { trace.push(`browser:${phase}`); },
      execute: () => 'browser-observed',
    });
    await runCoordinatedInvocation(native, {
      onPhase: (phase) => { trace.push(`native:${phase}`); },
      execute: () => 'native-observed',
    });
    expect(trace).toEqual([
      'browser:observe', 'browser:decide', 'browser:authorize', 'browser:verify',
      'native:observe', 'native:decide', 'native:authorize', 'native:verify',
    ]);
    expect(coordinateToolInvocation('computer_inspect', { target_application_id: 'https://example.test' }).error).toMatch(/browser tools/i);
  });
});
