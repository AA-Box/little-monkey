import { beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  resolveTarget: vi.fn(),
  snapshotForResolvedTarget: vi.fn(),
  effortForTarget: vi.fn(),
  beginDurableRun: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
  protectToolResult: vi.fn(),
  toolsForProfile: vi.fn(),
  isVisionCapableProviderModel: vi.fn(),
}));

vi.mock('./agentLoop', () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
  snapshotForResolvedTarget: (...args: unknown[]) => mocks.snapshotForResolvedTarget(...args),
}));

vi.mock('../store/modelStore', () => ({
  effortForTarget: (...args: unknown[]) => mocks.effortForTarget(...args),
}));

vi.mock('../store/workspaceStore', () => ({
  useWorkspaceStore: {
    getState: () => ({ roots: [{ id: 'root-1', path: '/workspace', label: 'workspace', is_primary: true }] }),
  },
}));

vi.mock('../store/permissionStore', () => ({
  usePermissionStore: { getState: () => ({ mode: 'default' }) },
}));

vi.mock('./durableRun', () => ({
  beginDurableRun: (...args: unknown[]) => mocks.beginDurableRun(...args),
}));

vi.mock('./tools', () => ({
  toolsForProfile: (...args: unknown[]) => mocks.toolsForProfile(...args),
}));

vi.mock('./turnEngine', () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
  executeToolCall: (...args: unknown[]) => mocks.executeToolCall(...args),
  isToolCallAllowed: (
    call: { function: { name: string } },
    tools: Array<{ function: { name: string } }>,
  ) => tools.some((tool) => tool.function.name === call.function.name),
  stringifyToolError: (error: unknown) =>
    JSON.stringify({ error: error instanceof Error ? error.message : String(error) }),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: 'Cancelled by the user' }),
}));

vi.mock('./untrustedContent', () => ({
  protectToolResult: (...args: unknown[]) => mocks.protectToolResult(...args),
}));

vi.mock('./visionModels', () => ({
  isVisionCapableProviderModel: (...args: unknown[]) => mocks.isVisionCapableProviderModel(...args),
}));

import { runHeadlessAgent, type RunHeadlessAgentParams } from './headlessAgentRunner';
import type { ToolCall } from './llamaClient';

const fakeTarget = { kind: 'local' as const, baseUrl: 'http://localhost:8090', modelLabel: 'Local' };

const recorder = {
  runId: 'durable-run-1',
  complete: vi.fn(),
  cancel: vi.fn(),
  fail: vi.fn(),
  recordUsage: vi.fn(),
  recordModelOutput: vi.fn(),
  recordToolProposed: vi.fn(),
  recordToolStarted: vi.fn(),
  recordToolFinished: vi.fn(),
};

function toolCall(name: string, id = 'call-1', args = '{"path":"src/index.ts"}'): ToolCall {
  return { id, type: 'function', function: { name, arguments: args } };
}

function baseParams(overrides: Partial<RunHeadlessAgentParams> = {}): RunHeadlessAgentParams {
  return {
    runId: 'run-1',
    signal: new AbortController().signal,
    systemPrompt: 'System instructions',
    userMessage: 'Do the work',
    maxIterations: 3,
    executionSource: 'test-source',
    durableRun: { task: 'Test background task', instructions: 'Test instructions' },
    ...overrides,
  };
}

beforeEach(() => {
  for (const mock of Object.values(mocks)) mock.mockReset();
  for (const mock of Object.values(recorder)) {
    if (typeof mock === 'function' && 'mockReset' in mock) mock.mockReset();
  }
  recorder.complete.mockResolvedValue(undefined);
  recorder.cancel.mockResolvedValue(undefined);
  recorder.fail.mockResolvedValue(undefined);
  recorder.recordToolProposed.mockResolvedValue(undefined);
  recorder.recordToolFinished.mockResolvedValue(undefined);
  mocks.resolveTarget.mockResolvedValue(fakeTarget);
  mocks.snapshotForResolvedTarget.mockReturnValue({ kind: 'local' });
  mocks.effortForTarget.mockReturnValue('medium');
  mocks.beginDurableRun.mockResolvedValue(recorder);
  mocks.toolsForProfile.mockImplementation((profile: 'explore' | 'code') => [
    { type: 'function', function: { name: 'read_file', parameters: {} } },
    ...(profile === 'code'
      ? [{ type: 'function', function: { name: 'run_shell', parameters: {} } }]
      : []),
  ]);
  mocks.protectToolResult.mockImplementation(
    (name: string, content: string) => `[protected:${name}]${content}`,
  );
  mocks.isVisionCapableProviderModel.mockReturnValue(false);
});

describe('runHeadlessAgent', () => {
  it('completes with the final model reply and records durable evidence', async () => {
    mocks.attemptStream.mockResolvedValue({
      content: 'Implemented and verified.',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
      usage: { promptTokens: 12, completionTokens: 4 },
    });

    const result = await runHeadlessAgent(baseParams());

    expect(result).toEqual({
      outcome: 'completed',
      summary: 'Implemented and verified.',
      durableRunId: 'durable-run-1',
    });
    expect(mocks.beginDurableRun).toHaveBeenCalledWith(expect.objectContaining({
      runId: 'run-1',
      kind: 'background',
      allowNetwork: false,
      allowExternalMutations: false,
      workspaceAccess: 'read_write',
    }));
    expect(recorder.recordUsage).toHaveBeenCalledWith(12, 4);
    expect(recorder.recordModelOutput).toHaveBeenCalledWith('run-1:0', 'Implemented and verified.');
    expect(recorder.complete).toHaveBeenCalledWith('Implemented and verified.');
  });

  it('offers only explore tools and records read-only workspace access for analysis runs', async () => {
    mocks.attemptStream.mockResolvedValue({
      content: 'Read-only analysis complete.',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    await runHeadlessAgent(baseParams({ toolProfile: 'explore' }));

    expect(mocks.toolsForProfile).toHaveBeenCalledWith('explore');
    const offeredTools = mocks.attemptStream.mock.calls[0][2] as Array<{ function: { name: string } }>;
    expect(offeredTools.map((tool) => tool.function.name)).toEqual(['read_file']);
    expect(mocks.beginDurableRun).toHaveBeenCalledWith(expect.objectContaining({
      workspaceAccess: 'read_only',
    }));
  });

  it('fails closed before streaming when multipart image content requires vision', async () => {
    const result = await runHeadlessAgent(baseParams({
      requireVision: true,
      userContent: [
        { type: 'text', text: 'Inspect this screenshot.' },
        { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA' } },
      ],
    }));

    expect(result).toMatchObject({ outcome: 'error', durableRunId: null });
    expect(result.summary).toContain('not configured as vision-capable');
    expect(mocks.attemptStream).not.toHaveBeenCalled();
    expect(mocks.beginDurableRun).not.toHaveBeenCalled();
  });

  it('passes multipart content through when the selected target has vision evidence', async () => {
    mocks.snapshotForResolvedTarget.mockReturnValue({
      kind: 'ollama',
      capabilities: { vision: { state: 'yes', evidence: 'fixture' } },
    });
    mocks.attemptStream.mockResolvedValue({
      content: 'Image inspected.',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    const userContent = [
      { type: 'text' as const, text: 'Inspect this screenshot.' },
      { type: 'image_url' as const, image_url: { url: 'data:image/png;base64,AAAA' } },
    ];

    await runHeadlessAgent(baseParams({ requireVision: true, userContent }));

    const history = mocks.attemptStream.mock.calls[0][1] as Array<{ role: string; content: unknown }>;
    expect(history[1]).toEqual({ role: 'user', content: userContent });
  });

  it('fails the durable run when feature-level final validation rejects the reply', async () => {
    mocks.attemptStream.mockResolvedValue({
      content: 'not valid feature output',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    const result = await runHeadlessAgent(baseParams({
      validateFinal: () => {
        throw new Error('Expected structured diagnosis JSON.');
      },
    }));

    expect(result.outcome).toBe('error');
    expect(result.summary).toBe('Expected structured diagnosis JSON.');
    expect(recorder.complete).not.toHaveBeenCalled();
    expect(recorder.fail).toHaveBeenCalledWith('Expected structured diagnosis JSON.');
  });

  it('executes allowed tools with source attribution and protects their result before the next model call', async () => {
    const call = toolCall('run_shell');
    const onToolActivity = vi.fn();
    mocks.attemptStream
      .mockResolvedValueOnce({ content: '', toolCalls: [call], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: 'Done.', toolCalls: [], streamError: null, contentStarted: true });
    mocks.executeToolCall.mockResolvedValue('{"stdout":"ok"}');

    const params = baseParams({ onToolActivity });
    const result = await runHeadlessAgent(params);

    expect(result.outcome).toBe('completed');
    expect(mocks.executeToolCall).toHaveBeenCalledWith(
      call,
      null,
      'run-1',
      expect.any(Map),
      params.signal,
      undefined,
      undefined,
      undefined,
      'test-source',
    );
    expect(onToolActivity).toHaveBeenCalledWith('run_shell');
    expect(recorder.recordToolProposed).toHaveBeenCalledWith(
      'call-1',
      'run_shell',
      '{"path":"src/index.ts"}',
    );
    expect(recorder.recordToolStarted).toHaveBeenCalledWith('call-1');
    expect(recorder.recordToolFinished).toHaveBeenCalledWith(
      'call-1',
      '{"stdout":"ok"}',
      expect.any(Number),
    );
    expect(mocks.protectToolResult).toHaveBeenCalledWith('run_shell', '{"stdout":"ok"}', false);
    const nextHistory = mocks.attemptStream.mock.calls[1][1] as Array<{ role: string; content: string }>;
    expect(nextHistory.find((message) => message.role === 'tool')?.content)
      .toBe('[protected:run_shell]{"stdout":"ok"}');
  });

  it('turns an unoffered tool into a denial result without executing it', async () => {
    mocks.attemptStream
      .mockResolvedValueOnce({ content: '', toolCalls: [toolCall('task')], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: 'Recovered.', toolCalls: [], streamError: null, contentStarted: true });

    const result = await runHeadlessAgent(baseParams());

    expect(result.outcome).toBe('completed');
    expect(mocks.executeToolCall).not.toHaveBeenCalled();
    expect(mocks.protectToolResult).not.toHaveBeenCalled();
    const nextHistory = mocks.attemptStream.mock.calls[1][1] as Array<{ role: string; content: string }>;
    expect(nextHistory.find((message) => message.role === 'tool')?.content)
      .toContain('Tool \\"task\\" was not offered to this run.');
  });

  it('denies tools that omit or escape a required attached worktree root', async () => {
    const call = toolCall('run_shell', 'call-root', '{"command":"pnpm test","cwd":"workspace"}');
    mocks.attemptStream
      .mockResolvedValueOnce({ content: '', toolCalls: [call], streamError: null, contentStarted: true })
      .mockResolvedValueOnce({ content: 'Stopped.', toolCalls: [], streamError: null, contentStarted: true });

    const result = await runHeadlessAgent(baseParams({ requiredWorkspaceRoot: 'debug-wt' }));

    expect(result.outcome).toBe('completed');
    expect(mocks.executeToolCall).not.toHaveBeenCalled();
    const nextHistory = mocks.attemptStream.mock.calls[1][1] as Array<{ role: string; content: string }>;
    expect(nextHistory.find((message) => message.role === 'tool')?.content)
      .toContain('may only target attached root \\"debug-wt\\"');
  });

  it('cancels before the first model call when the signal is already aborted', async () => {
    const controller = new AbortController();
    controller.abort();

    const result = await runHeadlessAgent(baseParams({ signal: controller.signal }));

    expect(result).toEqual({
      outcome: 'cancelled',
      summary: 'Cancelled by the user.',
      durableRunId: 'durable-run-1',
    });
    expect(mocks.attemptStream).not.toHaveBeenCalled();
    expect(recorder.cancel).toHaveBeenCalledWith('Cancelled by the user.');
  });

  it('returns and records a stream error', async () => {
    mocks.attemptStream.mockResolvedValue({
      content: '',
      toolCalls: [],
      streamError: 'provider unavailable',
      contentStarted: false,
    });

    const result = await runHeadlessAgent(baseParams());

    expect(result).toEqual({
      outcome: 'error',
      summary: 'provider unavailable',
      durableRunId: 'durable-run-1',
    });
    expect(recorder.fail).toHaveBeenCalledWith('provider unavailable');
  });

  it('stops at the configured tool-calling iteration cap', async () => {
    mocks.attemptStream.mockResolvedValue({
      content: '',
      toolCalls: [toolCall('read_file')],
      streamError: null,
      contentStarted: true,
    });
    mocks.executeToolCall.mockResolvedValue('contents');

    const result = await runHeadlessAgent(baseParams({ maxIterations: 2 }));

    expect(result.outcome).toBe('error');
    expect(result.summary).toBe(
      'Stopped after reaching the safety limit of 2 tool-calling iterations without a final answer.',
    );
    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(2);
    expect(recorder.fail).toHaveBeenCalledWith(result.summary);
  });
});
