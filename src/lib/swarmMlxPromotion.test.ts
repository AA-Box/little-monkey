import type {ModelInfo} from '../store/modelStore';
import {beforeEach,describe,expect,it,vi} from 'vitest';
import {MLX_PROMOTION_JOURNAL,promoteVerifiedMlxFusion,pendingMlxPromotion,
 recoverInterruptedMlxPromotion,type PromotionRuntime} from './swarmMlxPromotion';
const prev={id:'p',name:'Previous',path:'/models/old',runtime:'mlx',is_external:true} as ModelInfo;
const candidate={id:'c',name:'Trained',path:'/models/fused',runtime:'mlx',is_external:true} as ModelInfo;
let map:Map<string,string>,active:typeof candidate|null,running:string|null,runtime:PromotionRuntime;
const storage={
 getItem:(k:string)=>map.get(k)??null,
 setItem:(k:string,v:string)=>{map.set(k,v);},
 removeItem:(k:string)=>{map.delete(k);},
};
beforeEach(()=>{
 map=new Map();active=prev;running=prev.path;
 runtime={
  installed:()=>[prev,candidate],active:()=>active,
  activeProvider:()=> 'local',runningPath:()=>running,
  start:vi.fn(async m=>{active=m as typeof candidate;running=m.path;}),
  stop:vi.fn(async()=>{running=null;}),
  clearInactive:vi.fn(m=>{active=m as typeof candidate|null;}),
  probe:vi.fn(async path=>({modelPath:path,nonemptyOutputChars:6})),
 };
});
describe('verified MLX promotion and automatic live rollback',()=>{
 it('activates only after real target-fenced canary and clears transaction journal',async()=>{
  const receipt=await promoteVerifiedMlxFusion(candidate,runtime,storage);
  expect(receipt.nonemptyOutputChars).toBe(6);
  expect(runtime.start).toHaveBeenCalledWith(candidate);
  expect(runtime.probe).toHaveBeenCalledWith('/models/fused');
  expect(pendingMlxPromotion(storage)).toBeNull();
  expect(active).toBe(candidate);
 });
 it('restores previously running model if native canary rejects or changes model',async()=>{
  runtime.probe=vi.fn(async()=>({modelPath:'/models/wrong',nonemptyOutputChars:4}));
  await expect(promoteVerifiedMlxFusion(candidate,runtime,storage)).rejects.toThrow(/restored/);
  expect(runtime.start).toHaveBeenCalledTimes(2);
  expect(active).toBe(prev);expect(running).toBe('/models/old');
  expect(pendingMlxPromotion(storage)).toBeNull();
 });
 it('fail closes on failed rollback and recovers on next start',async()=>{
  let count=0;runtime.start=vi.fn(async m=>{
   if(++count===2)throw Error('Previous model temporarily offline');
   active=m as typeof candidate;running=m.path;
  });
  runtime.probe=vi.fn(async()=>{throw Error('Model timeout');});
  await expect(promoteVerifiedMlxFusion(candidate,runtime,storage)).rejects.toThrow(/routing is blocked/);
  expect(pendingMlxPromotion(storage)?.previousPath).toBe('/models/old');
  expect(await recoverInterruptedMlxPromotion(runtime,storage)).toBe(true);
  expect(active).toBe(prev);expect(pendingMlxPromotion(storage)).toBeNull();
 });
 it('does not activate an unregistered, ineligible or already selected model',async()=>{
  await expect(promoteVerifiedMlxFusion({...candidate,id:'rogue'} as never,runtime,storage)).rejects.toThrow(/registered/);
  runtime.activeProvider=()=> 'provider';
  await expect(promoteVerifiedMlxFusion(candidate,runtime,storage)).rejects.toThrow(/local MLX/);
  expect(runtime.start).not.toHaveBeenCalled();
 });
 it('refuses corrupted crash journal and keeps execution blocked',async()=>{
  map.set(MLX_PROMOTION_JOURNAL,'{');
  expect(()=>pendingMlxPromotion(storage)).toThrow(/execution is blocked/);
  await expect(promoteVerifiedMlxFusion(candidate,runtime,storage)).rejects.toThrow(/routing is blocked/);
 });
});
