import { describe, expect, it } from 'vitest';
import { parseSwarmReviewerReport, reconcileSwarmReviewers } from './swarmConsensus';

const vote = (id: string, verdict: 'support' | 'reject' | 'unknown', evidence: string[] = ['src/auth.ts:40']) =>
  JSON.stringify({ claims: [{ id, verdict, evidence }] });

describe('strict swarm reviewer reconciliation', () => {
  it('requires independent source-citing agreement, not mere model confidence', () => {
    const results = reconcileSwarmReviewers([
      { reviewer: 'Reviewer A', report: vote('auth-check', 'support') },
      { reviewer: 'Reviewer B', report: vote('auth-check', 'support', ['src/auth.ts:55']) },
    ]);
    expect(results.status).toBe('reviewer_agreement');
    expect(results.claims[0].supportingReviewers).toEqual(['Reviewer A', 'Reviewer B']);
    expect(results.claims[0].evidence).toContain('src/auth.ts:55');
  });
  it('blocks consensus when any reviewer disputes a claim', () => {
    const results = reconcileSwarmReviewers([
      { reviewer: 'Reviewer A', report: vote('auth-check', 'support') },
      { reviewer: 'Reviewer B', report: vote('auth-check', 'reject') },
    ]);
    expect(results.status).toBe('disputed');
    expect(results.claims[0].status).toBe('disputed');
  });
  it('fails closed for missing or malformed evidence and reviewer reports', () => {
    expect(reconcileSwarmReviewers([{ reviewer: 'a', report: 'looks good' }])).toEqual({
      status: 'inconclusive', parsedReviewerCount: 0, claims: [],
    });
    expect(reconcileSwarmReviewers([{ reviewer: 'A', report: vote('auth-check', 'support', []) }]).claims[0].status).toBe('insufficient_evidence');
    expect(parseSwarmReviewerReport('A', JSON.stringify({ claims: [
      { id: 'dup', verdict: 'support', evidence: [] },
      { id: 'dup', verdict: 'reject', evidence: [] },
    ] }))).toBeNull();
  });
  it('never mistakes malformed or executable model content for reviewer evidence', () => {
    expect(parseSwarmReviewerReport('A', ' ```json\n' + vote('claim-one', 'unknown') + '\n``` ')).not.toBeNull();
    expect(parseSwarmReviewerReport('A', '<script>steal()</script>')).toBeNull();
    expect(parseSwarmReviewerReport('A', '{"claims":[{"id":"../../../../etc/passwd","verdict":"support","evidence":[]}]}')).toBeNull();
  });
});
