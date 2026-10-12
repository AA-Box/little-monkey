import { beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  peersList: vi.fn(),
  peersOutbound: vi.fn(),
  peersRemoteThread: vi.fn(),
  peersCancelTask: vi.fn(),
  start: vi.fn(),
  recordToolCall: vi.fn(),
  appendMessage: vi.fn(),
  finish: vi.fn(),
  setSubagentRun: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke, isTauri: () => false }));
vi.mock('./peersClient', () => ({
  peersList: mocks.peersList,
  peersOutbound: mocks.peersOutbound,
  peersRemoteThread: mocks.peersRemoteThread,
  peersCancelTask: mocks.peersCancelTask,
}));
vi.mock('../store/subagentStore', () => ({
  useSubagentStore: { getState: () => ({
    start: mocks.start, recordToolCall: mocks.recordToolCall,
    appendMessage: mocks.appendMessage, finish: mocks.finish,
  }) },
}));
vi.mock('../store/sessionStore', () => ({
  useSessionStore: { getState: () => ({ setSubagentRun: mocks.setSubagentRun }) },
}));
import { remoteTaskCorrelation, remoteTaskOutcome, runPairedPeerAgent } from './swarmPeer';
import { assertReadOnlyPeerPlacement } from './swarmPeerSchema';
import { loadPeerObservations, PEER_METRICS_KEY } from './swarmPeerMetrics';

const task = {
  sessionId: 'session', workflowRunId: 'workflow',
  taskId: 'workflow#p0a0', description: 'Review auth',
  prompt: 'Inspect the authorization checks without editing files.',
  peer: 'paired-node', correlationRunId: 'workflow',
  correlationTaskPosition: 'p0a0',
};
const reply = (state: string, result_text: string | null = null) => ({
  alias: 'paired-node', message_id: 'm1', thread_id: 'thread-1',
  correlation_id: remoteTaskCorrelation('workflow', 'p0a0', task.prompt),
  kind: 'task_request', state, result_text, sent_at_ms: 1, checked_at_ms: 2,
});
beforeEach(() => {
  localStorage.removeItem(PEER_METRICS_KEY);
  for (const fn of Object.values(mocks)) fn.mockReset();
  mocks.peersList.mockResolvedValue({ inbound: [], outbound: [{ alias: 'paired-node', grants: ['task'] }] });
  mocks.peersOutbound.mockResolvedValue({ messages: [reply('accepted')] });
  mocks.peersRemoteThread.mockResolvedValue({ messages: [reply('succeeded', 'Authentication uses token verification')] });
  mocks.invoke.mockResolvedValue({ accepted: true });
  mocks.peersCancelTask.mockResolvedValue({ status: 'cancellation_requested' });
});

describe('paired peer swarm execution', () => {
  it('never reattaches to a stale remote result after a prompt change', () => {
    const oldKey = remoteTaskCorrelation('workflow', 'p0a0', task.prompt);
    const newKey = remoteTaskCorrelation('workflow', 'p0a0', task.prompt + ' Additional checks.');
    expect(newKey).not.toBe(oldKey);
    expect(remoteTaskCorrelation('workflow', 'p0a0', task.prompt)).toBe(oldKey);
    expect(remoteTaskOutcome([reply('succeeded', 'obsolete report')], newKey)).toEqual({ status: 'pending' });
  });

  it('accepts only explicitly named read-only peer placements', () => {
    expect(() => assertReadOnlyPeerPlacement('paired-node', 'explore')).not.toThrow();
    expect(() => assertReadOnlyPeerPlacement('paired-node', 'code')).toThrow(/read-only/);
    expect(() => assertReadOnlyPeerPlacement('paired-node', 'explore', true)).toThrow(/worktree/);
    expect(() => assertReadOnlyPeerPlacement('bad alias!', 'explore')).toThrow(/alias/);
  });
  it('attaches to existing durable receipt without resending the task', async () => {
    const result = await runPairedPeerAgent(task);
    expect(result).toContain('Untrusted data from paired peer');
    expect(result).toContain('Authentication uses token verification');
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.peersRemoteThread).toHaveBeenCalledWith('paired-node', 'thread-1');
    expect(mocks.finish).toHaveBeenCalledWith(task.taskId, 'done');
  });
  it('sends through the native bridge when no previous receipt exists', async () => {
    mocks.peersOutbound.mockResolvedValueOnce({ messages: [] });
    const result = await runPairedPeerAgent(task);
    expect(mocks.invoke).toHaveBeenCalledWith('peers_send_task', {
      alias: 'paired-node', text: task.prompt,
      correlation: remoteTaskCorrelation('workflow', 'p0a0', task.prompt),
    });
    expect(result).toContain('Authentication uses token verification');
  });
  it('refuses to double-dispatch when the bounded receipt history is saturated', async () => {
    const fullHistory = Array.from({ length: 200 }, (_, i) => ({
      ...reply('accepted'),
      correlation_id: 'unrelated-' + i,
      message_id: 'unrelated-message-' + i,
    }));
    mocks.peersOutbound.mockResolvedValueOnce({ messages: fullHistory });
    const result = await runPairedPeerAgent(task);
    expect(result).toContain('history is full');
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.peersRemoteThread).not.toHaveBeenCalled();
  });

  it('selects an online peer within the approved pool before a new dispatch', async () => {
    mocks.peersList.mockResolvedValue({ inbound: [], outbound: [
      { alias: 'paired-node', grants: ['task'], presence: 'offline' },
      { alias: 'node-b', grants: ['task'], presence: 'online' },
    ] });
    mocks.peersOutbound
      .mockResolvedValueOnce({ messages: [] })
      .mockResolvedValueOnce({ messages: [] })
      .mockResolvedValueOnce({ messages: [{ ...reply('accepted'), alias: 'node-b' }] });
    mocks.peersRemoteThread.mockResolvedValue({ messages: [{ ...reply('succeeded', 'actual result'), alias: 'node-b' }] });
    const output = await runPairedPeerAgent({ ...task, peerCandidates: ['paired-node', 'node-b'] });
    expect(mocks.invoke).toHaveBeenCalledWith('peers_send_task', expect.objectContaining({ alias: 'node-b' }));
    expect(output).toContain('actual result');
  });

  it('reattaches to the original receipt rather than moving a retry to a healthier peer', async () => {
    mocks.peersList.mockResolvedValue({ inbound: [], outbound: [
      { alias: 'paired-node', grants: ['task'], presence: 'offline' },
      { alias: 'node-b', grants: ['task'], presence: 'online' },
    ] });
    const result = await runPairedPeerAgent({ ...task, peerCandidates: ['paired-node', 'node-b'] });
    expect(result).toContain('Authentication uses token verification');
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.peersRemoteThread).toHaveBeenCalledWith('paired-node', 'thread-1');
  });

  it('does not contact unpaired peers or bypass missing task grants', async () => {
    mocks.peersList.mockResolvedValue({ inbound: [], outbound: [{ alias: 'paired-node', grants: ['message'] }] });
    const result = await runPairedPeerAgent(task);
    expect(result).toContain('error');
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.peersOutbound).not.toHaveBeenCalled();
  });
  it('marks failed and missing-result peer tasks as errors', () => {
    expect(remoteTaskOutcome([reply('failed', 'No peer-task recipe')], reply('failed').correlation_id!)).toEqual({
      status: 'error', reason: 'Remote task failed: No peer-task recipe',
    });
    expect(remoteTaskOutcome([reply('succeeded', null)], reply('succeeded').correlation_id!).status).toBe('error');
  });
  it('asks the original signed peer to cancel a running job after local abort', async () => {
    const controller = new AbortController();
    mocks.peersRemoteThread.mockImplementation(async () => {
      controller.abort();
      return { messages: [reply('accepted')] };
    });
    const result = await runPairedPeerAgent({ ...task, signal: controller.signal });
    expect(result).toContain('cancellation_requested');
    expect(mocks.peersCancelTask).toHaveBeenCalledWith('paired-node', 'thread-1', 'm1');
    expect(mocks.finish).toHaveBeenCalledWith(task.taskId, 'cancelled');
  });

  it('does not claim remote cancellation succeeded if the receiver cannot confirm it', async () => {
    const controller = new AbortController();
    mocks.peersCancelTask.mockRejectedValue(new Error('connection offline'));
    mocks.peersRemoteThread.mockImplementation(async () => {
      controller.abort();
      return { messages: [reply('accepted')] };
    });
    const result = await runPairedPeerAgent({ ...task, signal: controller.signal });
    expect(result).toContain('could not be confirmed');
    expect(result).toContain('still be running');
  });

  it('never sends a peer task after cancellation during peer authorization', async () => {
    const controller = new AbortController();
    mocks.peersList.mockImplementation(async () => {
      controller.abort();
      return { inbound: [], outbound: [{ alias: 'paired-node', grants: ['task'] }] };
    });
    const result = await runPairedPeerAgent({ ...task, signal: controller.signal });
    expect(result).toContain('cancelled before dispatch');
    expect(mocks.peersOutbound).not.toHaveBeenCalled();
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.finish).toHaveBeenCalledWith(task.taskId, 'cancelled');
  });

  it('never sends a peer task after cancellation during receipt lookup', async () => {
    const controller = new AbortController();
    mocks.peersOutbound.mockImplementationOnce(async () => {
      controller.abort();
      return { messages: [] };
    });
    const result = await runPairedPeerAgent({ ...task, signal: controller.signal });
    expect(result).toContain('no new work sent');
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.finish).toHaveBeenCalledWith(task.taskId, 'cancelled');
  });

  it('does not penalize peer reliability after uncertain network delivery', async () => {
    mocks.peersOutbound.mockResolvedValueOnce({ messages: [] });
    mocks.invoke.mockRejectedValueOnce(new Error('connection dropped after send'));
    const result = await runPairedPeerAgent(task);
    expect(result).toContain('temporarily unreachable');
    expect(loadPeerObservations()).toEqual({});
  });

  it('records only a receiver-confirmed terminal task failure', async () => {
    mocks.peersRemoteThread.mockResolvedValue({ messages: [reply('failed', 'Test failed on the peer')] });
    const result = await runPairedPeerAgent(task);
    expect(result).toContain('Test failed on the peer');
    expect(loadPeerObservations()['paired-node']?.failures).toBe(1);
  });

  it('aborts before dispatch and persists the local cancellation state', async () => {
    const controller = new AbortController(); controller.abort();
    await runPairedPeerAgent({ ...task, signal: controller.signal });
    expect(mocks.peersList).not.toHaveBeenCalled();
    expect(mocks.finish).toHaveBeenCalledWith(task.taskId, 'cancelled');
  });
});
