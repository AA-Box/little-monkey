import { beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

const apiMocks = vi.hoisted(() => ({
  executeExplicitDebugCommand: vi.fn(),
  diagnoseProductionIssue: vi.fn(),
  createProductionDebugWorktree: vi.fn(),
  runProductionDebugFix: vi.fn(),
}));

vi.mock('../lib/productionDebugging', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../lib/productionDebugging')>()),
  executeExplicitDebugCommand: (...args: unknown[]) => apiMocks.executeExplicitDebugCommand(...args),
  diagnoseProductionIssue: (...args: unknown[]) => apiMocks.diagnoseProductionIssue(...args),
  createProductionDebugWorktree: (...args: unknown[]) => apiMocks.createProductionDebugWorktree(...args),
  runProductionDebugFix: (...args: unknown[]) => apiMocks.runProductionDebugFix(...args),
}));

import {
  createProductionEvidence,
  notRunCommand,
  type ProductionDebugReport,
} from '../lib/productionDebugging';
import {
  __resetProductionDebugControllersForTests,
  PRODUCTION_DEBUG_STORAGE_KEY,
  useProductionDebuggingStore,
} from './productionDebuggingStore';

function fixtureEvidence(id = 'ev-1') {
  return createProductionEvidence({
    id,
    kind: 'error',
    origin: 'paste',
    label: 'Production error',
    sourceUri: 'paste://error',
    content: 'TypeError after deploy d-42',
    collectedAtMs: 1,
  });
}

function fixtureReport(): ProductionDebugReport {
  return {
    summary: 'Deploy d-42 introduced a null handling regression.',
    rootCauses: [{
      rank: 1,
      cause: 'Null handling regression',
      confidence: 'high',
      reasoning: 'The first error follows the deploy.',
      evidenceIds: ['ev-1'],
    }],
    evidenceLinks: [{ evidenceId: 'ev-1', label: 'Production error', sourceUri: 'paste://error', kind: 'error' }],
    reproduction: notRunCommand(),
    proposedPatch: { summary: 'Restore null handling.', files: ['src/api.ts'], diff: null, truncated: false },
    verification: notRunCommand(),
    unresolvedRisks: [],
    generatedAtMs: 1,
    diagnosisDurableRunId: 'diagnosis-1',
    fixDurableRunId: null,
    verificationDurableRunId: null,
  };
}

beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
  if (!('randomUUID' in globalThis.crypto)) {
    let counter = 0;
    Object.defineProperty(globalThis.crypto, 'randomUUID', {
      configurable: true,
      value: () => `debug-case-${++counter}`,
    });
  }
});

beforeEach(() => {
  localStorage.clear();
  for (const mock of Object.values(apiMocks)) mock.mockReset();
  __resetProductionDebugControllersForTests();
  useProductionDebuggingStore.setState({
    cases: [],
    selectedCaseId: null,
    activityByCase: {},
  });
});

function createCase() {
  const debugCase = useProductionDebuggingStore.getState().createCase({
    title: 'Widget API 500s',
    description: 'Started after deploy d-42.',
    repositorySlug: 'acme/widgets',
  });
  useProductionDebuggingStore.getState().attachEvidence(debugCase.id, fixtureEvidence());
  return debugCase;
}

describe('productionDebuggingStore', () => {
  it('persists bounded local cases and hydrates them after a store reset', () => {
    const debugCase = createCase();
    useProductionDebuggingStore.getState().attachEvidence(debugCase.id, {
      ...fixtureEvidence('ev-truncated'),
      truncated: true,
    });
    useProductionDebuggingStore.getState().updateCase(debugCase.id, {
      reproductionCommand: 'pnpm test repro',
      verificationCommand: 'pnpm test',
    });
    expect(localStorage.getItem(PRODUCTION_DEBUG_STORAGE_KEY)).toBeTruthy();

    useProductionDebuggingStore.setState({ cases: [], selectedCaseId: null });
    useProductionDebuggingStore.getState().init();

    const hydrated = useProductionDebuggingStore.getState().cases[0];
    expect(hydrated.title).toBe('Widget API 500s');
    expect(hydrated.evidence).toHaveLength(2);
    expect(hydrated.evidence.find((item) => item.id === 'ev-truncated')?.truncated).toBe(true);
    expect(hydrated.verificationCommand).toBe('pnpm test');
  });

  it('invalidates stale diagnosis and owned-worktree artifacts when diagnosis inputs change', () => {
    const debugCase = createCase();
    useProductionDebuggingStore.setState((state) => ({
      cases: state.cases.map((item) => item.id === debugCase.id
        ? {
            ...item,
            status: 'fix_prepared' as const,
            report: fixtureReport(),
            worktree: {
              worktreeId: 'wt-1',
              branch: 'production-debug/widget-api',
              workspaceLabel: 'debug-wt',
              canonicalPath: '/workspace-wt',
            },
          }
        : item),
    }));

    useProductionDebuggingStore.getState().updateCase(debugCase.id, { description: 'New incident facts.' });

    const updated = useProductionDebuggingStore.getState().cases[0];
    expect(updated.status).toBe('draft');
    expect(updated.report).toBeNull();
    expect(updated.worktree).toBeNull();
  });

  it('executes the explicit reproduction command and persists the real model diagnosis', async () => {
    const debugCase = createCase();
    useProductionDebuggingStore.getState().updateCase(debugCase.id, { reproductionCommand: 'pnpm test repro' });
    const commandEvidence = createProductionEvidence({
      id: 'ev-command',
      kind: 'command',
      origin: 'command',
      label: 'Reproduction command',
      sourceUri: 'command://repro',
      content: 'exit 1: reproduced',
    });
    apiMocks.executeExplicitDebugCommand.mockResolvedValue({
      execution: { status: 'failed', command: 'pnpm test repro', exitCode: 1, outputExcerpt: 'reproduced', evidenceId: 'ev-command', durableRunId: 'repro-1' },
      evidence: commandEvidence,
    });
    apiMocks.diagnoseProductionIssue.mockResolvedValue({
      outcome: 'completed',
      report: fixtureReport(),
      summary: fixtureReport().summary,
      durableRunId: 'diagnosis-1',
    });

    await useProductionDebuggingStore.getState().diagnose(debugCase.id);

    const updated = useProductionDebuggingStore.getState().cases[0];
    expect(updated.status).toBe('diagnosed');
    expect(updated.report?.rootCauses[0].cause).toContain('Null handling');
    expect(updated.evidence.some((item) => item.id === 'ev-command')).toBe(true);
    expect(apiMocks.executeExplicitDebugCommand).toHaveBeenCalledTimes(1);
    expect(apiMocks.diagnoseProductionIssue).toHaveBeenCalledTimes(1);
  });

  it('surfaces a diagnosis stream/model error without fabricating a report', async () => {
    const debugCase = createCase();
    apiMocks.diagnoseProductionIssue.mockResolvedValue({
      outcome: 'error',
      report: null,
      summary: 'provider unavailable',
      durableRunId: null,
    });

    await useProductionDebuggingStore.getState().diagnose(debugCase.id);

    const updated = useProductionDebuggingStore.getState().cases[0];
    expect(updated.status).toBe('failed');
    expect(updated.error).toBe('provider unavailable');
    expect(updated.report).toBeNull();
  });

  it('aborts an in-flight diagnosis and records cancellation', async () => {
    const debugCase = createCase();
    apiMocks.diagnoseProductionIssue.mockImplementation((params: { signal: AbortSignal }) =>
      new Promise((resolve) => {
        params.signal.addEventListener('abort', () => resolve({
          outcome: 'cancelled',
          report: null,
          summary: 'Cancelled by the user.',
          durableRunId: 'diagnosis-1',
        }), { once: true });
      }));

    const pending = useProductionDebuggingStore.getState().diagnose(debugCase.id);
    await vi.waitFor(() => expect(apiMocks.diagnoseProductionIssue).toHaveBeenCalledTimes(1));
    useProductionDebuggingStore.getState().cancel(debugCase.id);
    await pending;

    expect(useProductionDebuggingStore.getState().cases[0].status).toBe('cancelled');
  });

  it('prepares an owned-worktree patch, verification result, and durable run links without publishing', async () => {
    const debugCase = createCase();
    useProductionDebuggingStore.setState((state) => ({
      cases: state.cases.map((item) => item.id === debugCase.id
        ? { ...item, status: 'diagnosed' as const, report: fixtureReport() }
        : item),
    }));
    apiMocks.createProductionDebugWorktree.mockResolvedValue({
      worktreeId: 'wt-1',
      branch: 'production-debug/widget-api',
      workspaceLabel: 'debug-wt',
      canonicalPath: '/workspace-wt',
    });
    apiMocks.runProductionDebugFix.mockResolvedValue({
      outcome: 'completed',
      summary: 'Restored null handling.',
      durableRunId: 'fix-1',
      verification: { status: 'passed', command: 'pnpm test', exitCode: 0, outputExcerpt: '1 passed', evidenceId: 'verify-evidence', durableRunId: 'verify-1' },
      verificationEvidence: createProductionEvidence({
        id: 'verify-evidence',
        kind: 'command',
        origin: 'command',
        label: 'Verification command',
        sourceUri: 'command://verify-1',
        content: '1 passed',
      }),
      patch: { summary: 'Restore null handling.', files: ['src/api.ts'], diff: 'diff --git\n+guard null', truncated: false },
    });

    await useProductionDebuggingStore.getState().prepareFix(debugCase.id);

    const updated = useProductionDebuggingStore.getState().cases[0];
    expect(updated.status).toBe('fix_prepared');
    expect(updated.worktree?.branch).toBe('production-debug/widget-api');
    expect(updated.report?.proposedPatch.diff).toContain('guard null');
    expect(updated.report?.verification.status).toBe('passed');
    expect(updated.report?.fixDurableRunId).toBe('fix-1');
    expect(updated.report?.verificationDurableRunId).toBe('verify-1');
    expect(updated.evidence.some((item) => item.id === 'verify-evidence')).toBe(true);
  });
});
