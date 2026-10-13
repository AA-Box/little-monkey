// @vitest-environment jsdom
import {beforeEach,describe,it,expect} from 'vitest';
import {optimizeSwarmRoutingCosts,swarmCostOptimizationEnabled,setSwarmCostOptimization} from './swarmCostOptimizer';
import type {SwarmBenchmarkSummary} from './swarmVerifiedBenchmarks';
import type {CostUsageEntry} from '../store/costControlStore';
const ranking:SwarmBenchmarkSummary={
 generatedAt:'2026-10-12T00:00:00Z',importedAt:1,ranking:[
  {target:'provider:high',passes:15,attempts:15,accuracy:1,averageVerifiedMs:100},
  {target:'provider:cheap',passes:15,attempts:15,accuracy:1,averageVerifiedMs:200},
  {target:'provider:weak',passes:9,attempts:15,accuracy:0.6,averageVerifiedMs:100},
 ]};
const prices={
 'provider:high':{inputPerMillionUsd:6,outputPerMillionUsd:20},
 'provider:cheap':{inputPerMillionUsd:1,outputPerMillionUsd:3},
 'provider:weak':{inputPerMillionUsd:0.01,outputPerMillionUsd:0.01},
};
const entries:Array<CostUsageEntry>=Array.from({length:4},(_,i)=>({
 id:'sample'+i,occurredAtMs:Date.now()-i*1000,targetKey:'provider:high',
 targetLabel:'high',sessionId:'s',runId:null,
 usage:{promptTokens:800,completionTokens:200,totalTokens:1000},
 costUsd:null,
}));
const keys=['provider:high','provider:cheap','provider:weak'];
beforeEach(()=>localStorage.clear());
describe('operator opt-in bounded quality/cost optimization',()=>{
 it('is opt-in and uses only eligible high-quality candidates and real sample mix',()=>{
  expect(swarmCostOptimizationEnabled()).toBe(false);
  setSwarmCostOptimization(true);
  expect(swarmCostOptimizationEnabled()).toBe(true);
  const planned=optimizeSwarmRoutingCosts(keys,ranking,prices,entries,swarmCostOptimizationEnabled());
  expect(planned.sequence).toEqual(['provider:cheap','provider:high','provider:weak']);
  expect(planned.changed).toBe(true);
  expect(planned.estimatedSavingsFraction).toBeGreaterThan(0.7);
  expect(planned.evidenceSamples).toBe(4);
 });
 it('fails closed on missing evidence, unpriced baseline, explicit pins and no opt-in',()=>{
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,entries,false).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,entries,true,['provider:high']).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,entries.slice(0,2),true).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(keys,ranking,{'provider:cheap':prices['provider:cheap']},entries,true).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(keys,null,prices,entries,true).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(['provider:high'],ranking,prices,entries,true).changed).toBe(false);
 });
 it('cannot promote forbidden models, low quality, or unmeasured quality',()=>{
  const result=optimizeSwarmRoutingCosts(['provider:high','provider:weak'],ranking,prices,entries,true);
  expect(result.sequence).toEqual(['provider:high','provider:weak']);
  expect(optimizeSwarmRoutingCosts(['provider:high','provider:unknown'],ranking,prices,entries,true).changed).toBe(false);
  expect(optimizeSwarmRoutingCosts(['provider:high','provider:cheap','provider:cheap'],ranking,prices,entries,true).changed).toBe(false);
 });
 it('ignores old, future and forged usage instead of treating it as current spend',()=>{
  const stale=entries.map(row=>({...row,occurredAtMs:Date.now()-30*24*60*60_000}));
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,stale,true).changed).toBe(false);
  const future=entries.map(row=>({...row,occurredAtMs:Date.now()+20*60_000}));
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,future,true).changed).toBe(false);
  const invalid=entries.map(row=>({...row,occurredAtMs:NaN}));
  expect(optimizeSwarmRoutingCosts(keys,ranking,prices,invalid,true).changed).toBe(false);
 });
 it('does not switch on minor savings or invalid prices',()=>{
  const close={...prices,'provider:cheap':{inputPerMillionUsd:5.9,outputPerMillionUsd:19.8}};
  expect(optimizeSwarmRoutingCosts(keys,ranking,close,entries,true).changed).toBe(false);
  const bad={...prices,'provider:cheap':{inputPerMillionUsd:-1,outputPerMillionUsd:3}};
  expect(optimizeSwarmRoutingCosts(keys,ranking,bad,entries,true).changed).toBe(false);
  const noCost={...prices,'provider:high':{inputPerMillionUsd:0,outputPerMillionUsd:0}};
  expect(optimizeSwarmRoutingCosts(keys,ranking,noCost,entries,true).changed).toBe(false);
 });
});
