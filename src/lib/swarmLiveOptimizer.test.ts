// @vitest-environment jsdom
import {beforeEach,describe,expect,it} from 'vitest';
import {loadSwarmLiveOutcomes,recordSwarmLiveOutcome,selectLiveGuardedCostRoute,
 SWARM_LIVE_OUTCOMES_KEY,type SwarmLiveOutcome} from './swarmLiveOptimizer';
const NOW=1_800_000_000_000;
const original=['provider:primary','provider:cheap'];
const proposed=['provider:cheap','provider:primary'];
const rows=(target:string,status:'done'|'error',count:number,elapsedMs=1000):SwarmLiveOutcome[] =>
 Array.from({length:count},(_,i)=>({target,status,elapsedMs,finishedAtMs:NOW-5000+i}));
beforeEach(()=>localStorage.clear());
describe('continuous live operational guard (not a correctness oracle)',()=>{
 it('records only genuine resolved terminal outcomes, never cancelled or malformed input',()=>{
  recordSwarmLiveOutcome('provider:primary','done',1000,NOW);
  recordSwarmLiveOutcome('provider:primary','cancelled',2000,NOW);
  recordSwarmLiveOutcome('private-secret','error',200,NOW);
  recordSwarmLiveOutcome('provider:primary','error',Number.NaN,NOW);
  expect(loadSwarmLiveOutcomes(NOW)).toEqual([
   {target:'provider:primary',status:'done',elapsedMs:1000,finishedAtMs:NOW}
  ]);
 });
 it('keeps all traffic on measured baseline until six actual baseline outcomes exist',()=>{
  const limited=rows(original[0],'done',5);
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,0,limited))
   .toMatchObject({changed:false,phase:'insufficient-baseline',baselineSamples:5});
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,0,rows(original[0],'done',6)))
   .toMatchObject({changed:true,phase:'canary'});
 });
 it('rolls out 10%, then 30%, then 100% only when there is live evidence',()=>{
  const baseline=rows(original[0],'done',12);
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,1,baseline).changed).toBe(false);
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,0,baseline).changed).toBe(true);
  const ramp=[...baseline,...rows(original[1],'done',6)];
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,2,ramp))
   .toMatchObject({changed:true,phase:'ramp'});
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,3,ramp).changed).toBe(false);
  const established=[...baseline,...rows(original[1],'done',12)];
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,9,established))
   .toMatchObject({changed:true,phase:'established'});
 });
 it('stops routing immediately on production failures and latency regression',()=>{
  const baseline=rows(original[0],'done',12);
  const errors=[...baseline,...rows(original[1],'done',3),...rows(original[1],'error',3)];
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,0,errors))
   .toMatchObject({changed:false,phase:'regression-blocked'});
  const slow=[...baseline,...rows(original[1],'done',12,6000)];
  expect(selectLiveGuardedCostRoute(original,proposed,true,NOW,0,slow))
   .toMatchObject({changed:false,phase:'regression-blocked'});
 });
 it('will never invent or promote candidates outside the approved routing policy',()=>{
  const baseline=rows(original[0],'done',12);
  for(const option of [ ['provider:unauthorized','provider:primary'], ['provider:cheap'], 
    ['provider:cheap','provider:cheap'] ]){
    expect(selectLiveGuardedCostRoute(original,option,true,NOW,0,baseline).changed).toBe(false);
  }
  expect(selectLiveGuardedCostRoute(original,proposed,false,NOW,0,baseline).changed).toBe(false);
 });
 it('rejects stale/future/unbounded telemetry, corrupted storage and caps retention',()=>{
  localStorage.setItem(SWARM_LIVE_OUTCOMES_KEY,'{');
  expect(loadSwarmLiveOutcomes(NOW)).toEqual([]);
  const values=[...rows(original[0],'done',6),
   {...rows(original[0],'done',1)[0],finishedAtMs:NOW+90000},
   {...rows(original[1],'done',1)[0],elapsedMs:Infinity}];
  localStorage.setItem(SWARM_LIVE_OUTCOMES_KEY,JSON.stringify(values));
  expect(loadSwarmLiveOutcomes(NOW)).toHaveLength(6);
  for(let i=0;i<650;i++)recordSwarmLiveOutcome(original[0],'done',1000,NOW+i);
  expect(loadSwarmLiveOutcomes(NOW+650)).toHaveLength(600);
 });
});
