// @vitest-environment jsdom
import { beforeEach,describe,it,expect,vi } from 'vitest';
import { compiledReviewedSwarmPack,parseReviewedSwarmPack,installReviewedSwarmPack,
  removeReviewedSwarmPack,listReviewedSwarmPacks } from './swarmPackRegistry';

const valid={schema:1,id:'api-readiness',title:'API readiness',
  description:'Review API readiness',phases:[{title:'Read-only',
    agents:[{description:'API reviewer',prompt:'Inspect API documentation, cite code evidence.'}]}]};
beforeEach(()=>{
 const map=new Map<string,string>();
 vi.stubGlobal('localStorage',{getItem:(key:string)=>map.get(key)??null,
   setItem:(key:string,value:string)=>{map.set(key,value);}});
});
describe('data-only operator-reviewed pack registry',()=>{
 it('imports a bounded read-only pack and compiles a real validated workflow',()=>{
   installReviewedSwarmPack(valid);
   expect(listReviewedSwarmPacks()).toHaveLength(1);
   const workflow=compiledReviewedSwarmPack(listReviewedSwarmPacks()[0]);
   expect(workflow.phases[0].agents[0].profile).toBe('explore');
   expect(workflow.stopOnFailure).toBe(true);
   expect(workflow.phases[0].agents[0].peer).toBeUndefined();
   expect(workflow.phases[0].agents[0].isolation).toBeUndefined();
   expect(()=>installReviewedSwarmPack(valid)).toThrow(/already exists/);
   removeReviewedSwarmPack(valid.id);
   expect(listReviewedSwarmPacks()).toEqual([]);
 });
 it('rejects hooks, executable commands, profile escalation and remote placement',()=>{
   expect(()=>parseReviewedSwarmPack({...valid,install:'npm i unsafe'})).toThrow(/schema/);
   for(const field of [{profile:'code'},{peer:'remote'},{tools:['shell']},{isolation:'worktree'}]){
     const bad={...valid,phases:[{title:'Read-only',agents:[{...valid.phases[0].agents[0],...field}]}]};
     expect(()=>parseReviewedSwarmPack(bad)).toThrow(/only accept/);
   }
 });
 it('rejects oversized or malformed packs before storing or compiling',()=>{
   expect(()=>parseReviewedSwarmPack({...valid,id:'../unsafe'})).toThrow();
   expect(()=>parseReviewedSwarmPack({...valid,phases:Array(5).fill(valid.phases[0])})).toThrow();
   expect(()=>parseReviewedSwarmPack({...valid,phases:[{title:'x',agents:Array(5).fill(valid.phases[0].agents[0])}]})).toThrow();
   expect(()=>parseReviewedSwarmPack({...valid,phases:[{title:'x',agents:[{description:'a',prompt:'x'.repeat(2001)}]}]})).toThrow();
 });
});
