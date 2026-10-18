import {beforeEach,describe,expect,it,vi} from 'vitest';
import {
  readSemanticReviewQuality,recordSemanticReviewQuality,humanAgreementFraction,
} from './semanticMemoryQuality';
describe('private operator-observed semantic evaluation',()=>{
 const values=new Map<string,string>();
 beforeEach(()=>{
  values.clear();
  vi.stubGlobal('localStorage',{
   getItem:(key:string)=>values.get(key)??null,
   setItem:(key:string,val:string)=>{values.set(key,val);},
  });
 });
 it('tracks actual approvals and rejections without memory text, IDs or prompts',()=>{
  recordSemanticReviewQuality('proposed',3);
  recordSemanticReviewQuality('approved');
  const snapshot=recordSemanticReviewQuality('dismissed');
  expect(snapshot).toEqual({proposed:3,approved:1,dismissed:1});
  expect(humanAgreementFraction(snapshot)).toBe(0.5);
  const serialized=JSON.stringify([...values.values()]);
  expect(serialized).not.toContain('workspace');
  expect(serialized).not.toContain('fact');
  expect(readSemanticReviewQuality()).toEqual(snapshot);
 });
 it('fails closed for oversized counts and corrupted storage',()=>{
  expect(()=>recordSemanticReviewQuality('proposed',25)).toThrow();
  expect(humanAgreementFraction(readSemanticReviewQuality())).toBeNull();
  values.set('little-monkey-semantic-review-quality-v1','{');
  expect(readSemanticReviewQuality()).toEqual({proposed:0,approved:0,dismissed:0});
 });
});
