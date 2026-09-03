/**
 * `currentSystemPrompt` is the one place all four desktop prompt callers
 * (agentLoop, crewRunner, compareRunner, compareLabRunner) funnel through,
 * so it is where "a prompt was built from these memories" is recorded. This
 * suite pins that seam: the ids sent to `memory_mark_used` are exactly the
 * memories that appear under `## Remembered facts` in the prompt it returned
 * — not a superset, not a stale snapshot — and a rejecting backend still
 * yields a prompt.
 *
 * The exclusion guarantee itself (a disabled, expired or merge-retired
 * memory never reaches a prompt) is enforced and proved Rust-side, in
 * `memory.rs`'s `list_impl` tests and monkey-cli's prompt tests: by the time
 * a fact is in `rulesStore.facts` it has already passed that filter, and
 * this module deliberately adds no second filter of its own.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock('@tauri-apps/plugin-fs', () => ({ writeTextFile: vi.fn() }));

const facts = [
  { id: 'fact-pinned', text: 'Deploys are Thursday.', source: 'user' as const, created_at: '2026-01-01T00:00:00.000Z' },
  { id: 'fact-plain', text: 'Uses pnpm, not npm.', source: 'agent' as const, created_at: '2026-01-02T00:00:00.000Z' },
];

vi.mock('../store/rulesStore', () => ({
  useRulesStore: { getState: () => ({ rules: [], facts }) },
}));
vi.mock('../store/workspaceStore', () => ({
  useWorkspaceStore: {
    getState: () => ({ roots: [{ path: '/ws/project', label: 'project', is_primary: true }] }),
  },
}));
vi.mock('../store/mcpStore', () => ({ useMcpStore: { getState: () => ({ servers: [] }) } }));
vi.mock('../store/promptStore', () => ({ usePromptStore: { getState: () => ({ entries: [] }) } }));
vi.mock('../store/settingsStore', () => ({
  useSettingsStore: {
    getState: () => ({ webToolsEnabled: false, verifyEnabled: false, subagentsEnabled: false }),
  },
}));
vi.mock('../store/verifyStore', () => ({
  useVerifyStore: { getState: () => ({ config: { commands: [] } }) },
}));
vi.mock('../store/permissionStore', () => ({
  usePermissionStore: { getState: () => ({ mode: 'manual' }) },
}));

import { currentSystemPrompt } from './systemPrompt';

/** The `- text` lines under `## Remembered facts`, in prompt order. */
function rememberedFactLines(prompt: string): string[] {
  const start = prompt.indexOf('## Remembered facts');
  expect(start).toBeGreaterThan(-1);
  const lines: string[] = [];
  for (const line of prompt.slice(start).split('\n').slice(1)) {
    if (!line.startsWith('- ')) break;
    lines.push(line.slice(2));
  }
  return lines;
}

describe('currentSystemPrompt records which memories a prompt was built from', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(2);
  });

  it('marks exactly the memories that appear under ## Remembered facts', () => {
    const prompt = currentSystemPrompt();

    expect(rememberedFactLines(prompt)).toEqual(facts.map((fact) => fact.text));
    expect(invokeMock).toHaveBeenCalledWith('memory_mark_used', {
      ids: ['fact-pinned', 'fact-plain'],
    });
  });

  it('returns a prompt and throws nothing when the backend rejects', async () => {
    invokeMock.mockRejectedValue(new Error('unknown command'));
    expect(() => currentSystemPrompt()).not.toThrow();
    expect(currentSystemPrompt()).toContain('## Remembered facts');
    // Flush the microtask queue so an unhandled rejection would surface here.
    await Promise.resolve();
    await Promise.resolve();
  });
});
