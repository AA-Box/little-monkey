import {describe,expect,it} from 'vitest';
import {
  buildSemanticReviewPrompt,reviewEligibleFacts,validateSemanticReview,intersectIndependentSemanticReviews,
  reviewScopeKey,configuredSemanticVerifiers,distinctVerifierOrThrow,
} from './semanticMemoryReview';
import type {MemoryEntry} from './memoryStudio';
import {useModelStore} from '../store/modelStore';

const f=(id:string,text:string,extra:Partial<MemoryEntry>={}):MemoryEntry=>({
 id,text,source:'agent',enabled:true,created_at:'2026-01-01T00:00:00Z',
 source_turn_id:null,pinned:false,expires_at:null,last_used_at:null,
 merged_from:[],merged_into:null,retired_at:null,
 scope:'project',project_root:'/repo/a',...extra,
});
const facts=[
 f('a','User likes short summaries.'),
 f('b','User prefers brief recaps.'),
 f('c','User prefers detailed explanations.'),
];

describe('independent model identity gate',()=>{
 it('does not allow the selected model to verify its own proposal',()=>{
  const original=useModelStore.getState();
  try{
   useModelStore.setState({
    providers:[{id:'provider-a',label:'Provider A',has_key:true,is_extension:false},
               {id:'provider-b',label:'Provider B',has_key:true,is_extension:false}] as typeof original.providers,
    providerModels:{'provider-a':[{id:'model-a'}],'provider-b':[{id:'model-b'}]} as typeof original.providerModels,
   });
   const models=configuredSemanticVerifiers();
   expect(models).toHaveLength(2);
   expect(()=>distinctVerifierOrThrow(
     {kind:'provider',providerId:'provider-a',model:'model-a'},models[0].key,
   )).toThrow(/different model/);
   expect(distinctVerifierOrThrow(
     {kind:'provider',providerId:'provider-a',model:'model-a'},models[1].key,
   )).toEqual({kind:'provider',providerId:'provider-b',model:'model-b'});
   expect(()=>distinctVerifierOrThrow({kind:'local',baseUrl:'http://127.0.0.1:8090'},'other'))
     .toThrow(/Select a configured/);
  }finally{useModelStore.setState({providers:original.providers,providerModels:original.providerModels});}
 });
});

describe('untrusted semantic memory candidate validation',()=>{
 it('permits genuine paraphrase suggestions with approval required and exact original snapshots',()=>{
  const prompt=buildSemanticReviewPrompt(facts);
  expect(prompt[1].role).toBe('user');
  expect(JSON.stringify(prompt)).toContain('User likes short summaries.');
  expect(JSON.stringify(prompt)).not.toContain('tool_call');
  const candidates=validateSemanticReview({proposals:[
   {kind:'merge',ids:['a','b'],suggestedText:'User prefers concise summaries.',
    reason:'Paraphrases of the same preference'},
   {kind:'contradiction',ids:['a','c'],reason:'Different preferences for answer detail'},
  ]},facts);
  expect(candidates).toHaveLength(2);
  expect(candidates[0]).toEqual({
   kind:'merge',ids:['a','b'],suggestedText:'User prefers concise summaries.',
   reason:'Paraphrases of the same preference',
   expectedTexts:['User likes short summaries.','User prefers brief recaps.'],
   scopeKey:'project:/repo/a',
  });
  expect(candidates[1]).toMatchObject({kind:'contradiction',ids:['a','c']});
 });
 it('requires separate proposal and verification to agree on exact scoped identities',()=>{
  const proposer=validateSemanticReview({proposals:[
   {kind:'merge',ids:['a','b'],suggestedText:'User likes concise summaries.',
    reason:'Paraphrase'},
   {kind:'contradiction',ids:['a','c'],reason:'Different requests'},
  ]},facts);
  const verifier=validateSemanticReview({proposals:[
   {kind:'merge',ids:['b','a'],suggestedText:' USER LIKES CONCISE SUMMARIES. ',
    reason:'Same enduring claim'},
   {kind:'merge',ids:['a','c'],suggestedText:'Conflicting claims',
    reason:'Improper merge'},
  ]},[...facts].reverse());
  const shared=intersectIndependentSemanticReviews(proposer,verifier);
  expect(shared).toHaveLength(1);
  expect(shared[0].kind).toBe('merge');
  expect(shared[0].ids).toEqual(['a','b']);
  const unsafe=verifier.map(item=>item.kind==='merge'&&item.ids.includes('b')
   ? {...item,suggestedText:'Different meaning entirely.'} : item);
  expect(intersectIndependentSemanticReviews(proposer,unsafe)).toEqual([]);

  expect(intersectIndependentSemanticReviews(proposer,[])).toEqual([]);
 });
 it('rejects forged IDs, cross-workspace and user-pinned/disabled fact edits',()=>{
  const other=f('different','User likes short summaries.',{project_root:'/repo/b'});
  expect(()=>reviewEligibleFacts([facts[0],other])).toThrow(/one scope/);
  expect(()=>reviewEligibleFacts([facts[0],f('pin','Pinned preference',{pinned:true})]))
   .toThrow(/unpinned/);
  expect(()=>reviewEligibleFacts([facts[0],f('user','User authored',{source:'user'})]))
   .toThrow(/agent/);
  expect(validateSemanticReview({proposals:[
   {kind:'merge',ids:['a','foreign'],suggestedText:'Unsafe mutation',reason:'guess'},
   {kind:'contradiction',ids:['a','a'],reason:'duplicate identity'},
   {kind:'merge',ids:['a','b'],suggestedText:'API_KEY=secret',reason:'bad secret'},
   {kind:'merge',ids:['a','b'],suggestedText:'User prefers concise summaries.',
    reason:'Candidate survives'},
  ]},facts)).toHaveLength(1);
 });
 it('never accepts unchecked model output quantity or malformed identity lists',()=>{
  expect(()=>validateSemanticReview({proposals:Array(9).fill({})},facts))
   .toThrow(/too many/);
  expect(()=>validateSemanticReview([],facts)).toThrow(/bounded/);
  expect(validateSemanticReview({proposals:[
   {kind:'merge',ids:['a'],suggestedText:'Short',reason:'missing second id'},
   {kind:'contradiction',ids:['a','b','c'],reason:'wrong pair cardinality'},
  ]},facts)).toHaveLength(0);
  expect(reviewScopeKey(facts[0])).toBe('project:/repo/a');
 });
 it('never uses secrets or unsupported external tool metadata in review',()=>{
  expect(()=>reviewEligibleFacts([facts[0],f('key','Store API_KEY=abc',{})]))
   .toThrow(/unpinned/);
 });
});
