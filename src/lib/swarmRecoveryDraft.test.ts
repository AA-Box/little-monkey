import { describe, expect, it } from 'vitest';
import { prepareSwarmResumeDraft } from './swarmRecoveryDraft';
import type { WorkflowRunMeta } from '../store/sessionStore';

function saved(profile = 'explore', status: WorkflowRunMeta['status'] = 'error'): WorkflowRunMeta {
  return {
    name: 'Repository audit',
    description: 'Inspect the repository',
    status, startedAt: 1, finishedAt: 2,
    phases: [{ title: 'Review', agents: [{ taskId: 'old-task', description: 'Inspector' }] }],
    spec: { name: 'Repository audit', description: 'Inspect the repository',
      phases: [{ title: 'Review', agents: [{ description: 'Inspector', prompt: 'Inspect files', profile }] }] },
    agentResults: {},
  };
}

describe('operator-reviewed workflow recovery', () => {
  it('prepares a read-only resume without invoking or reconstructing a new graph', () => {
    const draft = prepareSwarmResumeDraft('call_old', saved());
    expect(draft.requiresMutationReview).toBe(false);
    expect(draft.text).toContain('"resume": "call_old"');
    expect(draft.text).toContain('this conversation');
    expect(draft.text).not.toContain('mode":"implement"');
  });
  it('requires a repository-side-effect inspection for code-capable graphs', () => {
    const draft = prepareSwarmResumeDraft('call_old', saved('code', 'cancelled'));
    expect(draft.requiresMutationReview).toBe(true);
    expect(draft.text).toContain('repository status');
    expect(draft.text).toContain('permission policy');
  });
  it('fails closed for completed, running, missing and invalid saved runs', () => {
    expect(() => prepareSwarmResumeDraft('call_done', saved('explore', 'done'))).toThrow(/Only interrupted/);
    expect(() => prepareSwarmResumeDraft('call_running', saved('explore', 'running'))).toThrow(/Only interrupted/);
    expect(() => prepareSwarmResumeDraft('../unsafe', saved())).toThrow(/Invalid/);
    expect(() => prepareSwarmResumeDraft('call_empty', { ...saved(), spec: undefined })).toThrow(/recoverable/);
  });
});
