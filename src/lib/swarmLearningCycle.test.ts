// @vitest-environment jsdom
import {beforeEach,describe,expect,it,vi} from 'vitest';
import {distillLearningCandidate,listLearningPatterns,decideLearningCandidate,
 approvedLearningContext,forgetLearningPattern} from './swarmLearningCycle';
import type {VerifiedSpecialistSummary} from './swarmVerifiedSpecialists';
const summary:VerifiedSpecialistSummary={
  domain:'backend',target:'ollama:local',generatedAt:new Date().toISOString(),
  importedAt:Date.now(),ranking:[{specialist:'API resilience reviewer',passes:8,attempts:9,
    accuracy:0.8889,averageVerifiedMs:100}],
};
beforeEach(()=>{
 const map=new Map<string,string>();
 vi.stubGlobal('localStorage',{getItem:(k:string)=>map.get(k)??null,
  setItem:(k:string,v:string)=>{map.set(k,v);},removeItem:(k:string)=>map.delete(k)});
});
describe('review-gated learning cycle',()=>{
 it('judges complete measured fixtures, distills a candidate but never auto-consolidates',()=>{
  const candidate=distillLearningCandidate(summary);
  expect(candidate?.status).toBe('pending');
  expect(approvedLearningContext('backend')).toBe('');
  expect(listLearningPatterns()).toHaveLength(1);
  decideLearningCandidate(candidate!.id,candidate!.revision,'approved');
  expect(approvedLearningContext('backend')).toContain('untrusted');
  expect(approvedLearningContext('frontend')).toBe('');
  forgetLearningPattern(candidate!.id);
  expect(approvedLearningContext('backend')).toBe('');
 });
 it('rejects completion-only results and insufficient independently verified grades',()=>{
  expect(distillLearningCandidate({...summary,ranking:[{...summary.ranking[0],attempts:3,passes:3}]})).toBeNull();
  expect(distillLearningCandidate({...summary,ranking:[{...summary.ranking[0],passes:2,accuracy:0.2222}]})).toBeNull();
 });
 it('does not silently replace an approved version with a new pending one',()=>{
  const first=distillLearningCandidate(summary)!;
  decideLearningCandidate(first.id,first.revision,'approved');
  const second=distillLearningCandidate(summary)!;
  expect(second.revision).toBe(first.revision+1);
  expect(approvedLearningContext('backend')).toContain('approved revision: 1');
  expect(()=>decideLearningCandidate(first.id,first.revision,'approved')).toThrow(/already reviewed/);
 });
 it('does not retrieve stale learning',()=>{
  const first=distillLearningCandidate(summary)!;
  decideLearningCandidate(first.id,first.revision,'approved');
  expect(approvedLearningContext('backend',Date.now()+91*24*60*60_000)).toBe('');
 });
});
