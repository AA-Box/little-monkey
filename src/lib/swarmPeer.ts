/**
 * Explicit, permission-bounded remote read-only workflow tasks.
 *
 * Sending uses the existing paired-peer daemon protocol. The peer controls
 * its own recipe, tools, workspace and approvals; the sender never gets to
 * select or expand them. Results are returned as untrusted evidence.
 */
import { invoke } from '@tauri-apps/api/core';
import { useSubagentStore } from '../store/subagentStore';
import { useSessionStore } from '../store/sessionStore';
import { peersCancelTask, peersList, peersOutbound, peersRemoteThread, type PeerOutboundMessage } from './peersClient';
import { wrapUntrustedContent } from './untrustedContent';
import { validSwarmPeer } from './swarmPeerSchema';
import { chooseSwarmPeer, loadPeerObservations, recordPeerObservation } from './swarmPeerMetrics';

export const REMOTE_TASK_TIMEOUT_MS = 5 * 60_000;
export const REMOTE_POLL_INTERVAL_MS = 2_500;
export const REMOTE_REPORT_MAX_CHARS = 8_000;
export function remoteTaskCorrelation(runId: string, taskId: string, prompt = ''): string {
  // A stable correlation allows a resumed workflow to reconnect to its own
  // durable outbound receipt rather than sending the same work twice.
  let left = 0x811c9dc5;
  let right = 0x5bd1e995;
  for (const ch of runId + '\u0000' + taskId + '\u0000' + prompt) {
    left = Math.imul(left ^ ch.charCodeAt(0), 0x01000193);
    right = Math.imul(right ^ ch.charCodeAt(0), 0x85ebca6b);
  }
  return 'swarm-' + (left >>> 0).toString(16).padStart(8, '0') + (right >>> 0).toString(16).padStart(8, '0');
}

export type RemoteTaskOutcome =
  | { status: 'pending' }
  | { status: 'done'; report: string }
  | { status: 'error'; reason: string };

export function remoteTaskOutcome(messages: readonly PeerOutboundMessage[], correlation: string): RemoteTaskOutcome {
  const relevant = messages.find(m => m.correlation_id === correlation);
  if (!relevant) return { status: 'pending' };
  switch (relevant.state) {
    case 'succeeded':
      return relevant.result_text?.trim()
        ? { status: 'done', report: relevant.result_text.slice(0, REMOTE_REPORT_MAX_CHARS) }
        : { status: 'error', reason: 'Remote task succeeded without a readable result.' };
    case 'rejected':
    case 'failed':
    case 'cancelled':
      return { status: 'error', reason: 'Remote task ' + relevant.state + ': ' + (relevant.result_text ?? 'no details') };
    default:
      return { status: 'pending' };
  }
}

export interface RemoteAgentTask {
  sessionId: string;
  workflowRunId: string;
  taskId: string;
  description: string;
  prompt: string;
  peer: string;
  /** Optional operator-approved alternative aliases, never discovered automatically. */
  peerCandidates?: string[];
  correlationRunId: string;
  /** Stable task position across resumed workflow tool-call ids. */
  correlationTaskPosition: string;
  signal?: AbortSignal;
}

function stopped(signal?: AbortSignal): boolean {
  return signal?.aborted === true;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (stopped(signal)) return Promise.resolve();
  return new Promise(resolve => {
    const timer = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
    const onAbort = () => { clearTimeout(timer); signal?.removeEventListener('abort', onAbort); resolve(); };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

export async function runPairedPeerAgent(task: RemoteAgentTask): Promise<string> {
  const { sessionId, workflowRunId, taskId, description, prompt, peer: initialPeer, peerCandidates, correlationRunId, correlationTaskPosition, signal } = task;
  let peer = initialPeer;
  let latestError = '';
  const error = (message: string) => {
    latestError = message.slice(0, 600);
    return JSON.stringify({ error: wrapUntrustedContent('paired peer task result', latestError) });
  };
  if (!validSwarmPeer(peer) || (peerCandidates && (peerCandidates.length > 3 || !peerCandidates.length ||
      !peerCandidates.every(validSwarmPeer) || !peerCandidates.includes(peer)))) {
    return error('Invalid explicitly selected paired peer pool.');
  }
  if (prompt.length > 7_000) return error('Remote task context exceeds the 7000-character peer limit.');
  // Include the exact composed prompt: a changed task must NEVER inherit
  // a successful result from a previous iteration at the same graph position.
  const correlation = remoteTaskCorrelation(correlationRunId, correlationTaskPosition, prompt);
  const startedAt = Date.now();
  const store = useSubagentStore.getState();
  store.start({ sessionId, taskId, cancelId: '', workflowRunId, description, profile: 'explore' });
  let terminalObserved = false;
  let result = '';
  let status: 'done' | 'error' | 'cancelled' = 'error';
  try {
    if (stopped(signal)) {
      status = 'cancelled';
      return error('Remote task cancelled before dispatch.');
    }
    const { outbound } = await peersList();
    if (stopped(signal)) {
      status = 'cancelled';
      return error('Remote task cancelled before dispatch.');
    }
    const pool = peerCandidates?.length ? peerCandidates : [peer];
    const granted = outbound.filter(node => pool.includes(node.alias) && node.grants.includes('task'));
    if (!granted.length) return error('No selected peer has an active task-request grant.');
    // Inspect EVERY candidate's durable receipts before choosing a new
    // node. A previously dispatched task must reattach to its original
    // receiver even if health or performance rankings have since changed.
    let existing: PeerOutboundMessage | undefined;
    for (const candidate of pool) {
      if (stopped(signal)) {
        status = 'cancelled';
        return error('Remote task cancelled before dispatch; no new work sent.');
      }
      const records = await peersOutbound(candidate, 200);
      const match = records.messages.find(m => m.correlation_id === correlation && m.kind === 'task_request');
      if (match) {
        if (!granted.some(node => node.alias === candidate)) return error('Previously selected peer no longer has a task grant; cannot safely re-dispatch.');
        peer = candidate;
        existing = match;
        break;
      }
      if (records.messages.length >= 200) return error('Peer outbound history is full; cannot safely prove this task was not already sent.');
    }
    if (!existing) {
      if (stopped(signal)) {
        status = 'cancelled';
        return error('Remote task cancelled before dispatch; no new work sent.');
      }
      const candidate = chooseSwarmPeer(pool, granted, loadPeerObservations());
      if (!candidate) return error('No selected task-granted peer is currently healthy enough for dispatch.');
      peer = candidate;
      // After the final authorization and receipt check, stop on an abort
      // before attempting any new transmission.
      if (stopped(signal)) {
        status = 'cancelled';
        return error('Remote task cancelled before dispatch; no new work sent.');
      }
      await invoke('peers_send_task', { alias: peer, text: prompt, correlation });
      const sent = await peersOutbound(peer, 200);
      existing = sent.messages.find(m => m.correlation_id === correlation && m.kind === 'task_request');
    }
    if (!existing) return error('Peer dispatch has no durable outbound receipt.');
    store.recordToolCall(taskId, 'paired peer ' + peer);
    const threadId = existing.thread_id;
    const deadline = Date.now() + REMOTE_TASK_TIMEOUT_MS;
    while (Date.now() <= deadline) {
      if (stopped(signal)) {
        status = 'cancelled';
        try {
          const reply = await peersCancelTask(peer, existing.thread_id, existing.message_id);
          return error('Remote cancellation ' + reply.status + '. This is not proof the worker has stopped; verify the terminal result.');
        } catch (cause) {
          return error('Local wait stopped but remote cancellation could not be confirmed: ' + String(cause).slice(0, 220) +
            '. The peer may still be running; inspect the durable outbound thread.');
        }
      }
      const messages = await peersRemoteThread(peer, threadId);
      const outcome = remoteTaskOutcome(messages.messages, correlation);
      if (outcome.status === 'done') {
        terminalObserved = true;
        status = 'done';
        result = wrapUntrustedContent('paired peer ' + peer, outcome.report);
        return result;
      }
      if (outcome.status === 'error') {
        // Receiver-reported cancellation does not measure agent quality.
        terminalObserved = !outcome.reason.startsWith('Remote task cancelled');
        return error(outcome.reason);
      }
      await sleep(REMOTE_POLL_INTERVAL_MS, signal);
    }
    return error('Remote task is still pending after timeout. Its durable outbound record remains available for resume.');
  } catch (cause) {
    // A network error after dispatch is not proof the peer never received the
    // task. Retrying a workflow will reattach to the existing local receipt.
    return error('Peer task failed or is temporarily unreachable: ' + String(cause));
  } finally {
    // Timeouts, lost connections and uncertain dispatch are NOT evidence of
    // agent/model failure; only a receiver-confirmed terminal outcome counts.
    if (terminalObserved) recordPeerObservation(peer, status, Date.now() - startedAt);
    const output = result || (latestError ? wrapUntrustedContent('paired peer task result', latestError) : 'Remote task failed or timed out.');
    useSubagentStore.getState().appendMessage(taskId, { role: 'assistant', content: output });
    useSubagentStore.getState().finish(taskId, status);
    useSessionStore.getState().setSubagentRun(sessionId, taskId, [
      { role: 'user', content: prompt },
      { role: 'assistant', content: output },
    ], {
      status,
      workflowRunId,
      description,
      profile: 'explore',
      startedAt,
      finishedAt: Date.now(),
      toolCallCount: 1,
    });
  }
}
