import { describe, expect, it } from 'vitest';
import { approvedNotebookVectorClaim } from './swarmNotebookVector';
import type { SwarmNotebookNote } from './swarmNotebook';

function note(status: 'approved'|'pending'|'rejected'='approved'): SwarmNotebookNote {
  return {
    project:'/workspace/project-alpha',
    id:'auth-boundary',
    activeVersion:status==='approved'?1:null,
    revisions:[{
      version:1,status,createdAt:123,sourceRunId:'workflow-1',
      reviewers:['independent-a','independent-b'],
      evidence:['src/auth.ts:41','src/router.ts:15'],
    }],
  };
}
describe('operator-gated notebook to replicated vector export',()=>{
  it('refuses pending or rejected model suggestions',()=>{
    expect(()=>approvedNotebookVectorClaim(note('pending'))).toThrow(/approved/);
    expect(()=>approvedNotebookVectorClaim(note('rejected'))).toThrow(/approved/);
  });
  it('builds bounded non-executable evidence without leaking the project root',()=>{
    const result=approvedNotebookVectorClaim(note());
    expect(result.id).toMatch(/^review-auth-boundary-[0-9a-f]{8}$/);
    expect(result.text).toContain('src/auth.ts:41');
    expect(result.text).not.toContain('/workspace/project-alpha');
    expect(result.approvedVersion).toBe(1);
    expect(approvedNotebookVectorClaim(note()).id).toBe(result.id);
  });
  it('isolates same-named claim IDs in independent project namespaces',()=>{
    const one=note(),two={...note(),project:'/different/project'};
    expect(approvedNotebookVectorClaim(one).id).not.toBe(approvedNotebookVectorClaim(two).id);
  });
  it('fails safely on oversized or malformed approved evidence',()=>{
    const corrupted=note();
    corrupted.revisions[0].evidence=['x'.repeat(3000)];
    expect(()=>approvedNotebookVectorClaim(corrupted)).toThrow(/limit/);
    const noEvidence=note();
    noEvidence.revisions[0].evidence=[];
    expect(()=>approvedNotebookVectorClaim(noEvidence)).toThrow(/evidence/);
  });
});
