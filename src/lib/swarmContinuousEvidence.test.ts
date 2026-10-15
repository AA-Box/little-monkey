// @vitest-environment jsdom
import {beforeEach,describe,expect,it,vi} from 'vitest';
vi.mock('@tauri-apps/api/core',()=>({isTauri:()=>false}));
vi.mock('@tauri-apps/api/path',()=>({appDataDir:async()=>'/tmp/app',join:async(...s:string[])=>s.join('/')}));
vi.mock('@tauri-apps/plugin-fs',()=>({readTextFile:vi.fn()}));
import {validateContinuousQualityReceipt} from './swarmContinuousEvidence';
import {
  importSwarmBenchmark,loadSwarmBenchmark,
  setSwarmContinuousQuality,invalidateSwarmContinuousQuality,
} from './swarmVerifiedBenchmarks';
const NOW=Date.parse('2026-10-14T10:00:00Z');
const FIX=['logic-regression','authorization-regression','parsing-regression'];
function source(){
 const target='ollama:local-reviewed';
 const results=Array.from({length:3},(_,i)=>FIX.map((fixture,j)=>({
  target,fixture,repeat:i+1,runId:'run-local-reviewed-'+i+'-'+j,
  verified:true,durationMs:500,
 }))).flat();
 return {version:1,mode:'real-model',benchmark:'live-task-verified-fixtures',
  generatedAt:new Date(NOW).toISOString(),results,
  ranking:[{target,passes:9,attempts:9,accuracy:1,averageVerifiedMs:500}]};
}
function receipt(report:unknown=source()){
 return {version:1,source:'native_disposable_repository_independent_tests',
   status:'verified',createdAt:new Date(NOW).toISOString(),
   validUntil:new Date(NOW+36*60*60_000).toISOString(),
   regressionTargets:[],report};
}
beforeEach(()=>localStorage.clear());
describe('unattended independent quality intake and operator gating',()=>{
 it('accepts current, complete repeated grades but never claims own-answer verification',()=>{
  expect(validateContinuousQualityReceipt(receipt(),NOW).summary.ranking[0].accuracy).toBe(1);
  importSwarmBenchmark(source(),NOW,true);
  setSwarmContinuousQuality(true);
  expect(loadSwarmBenchmark(NOW)?.ranking[0].attempts).toBe(9);
  expect(loadSwarmBenchmark(NOW+36*60*60_000+1000)).toBeNull();
 });
 it('requires independent receipt in continuous mode, not yesterday manual score',()=>{
  importSwarmBenchmark(source(),NOW);
  expect(loadSwarmBenchmark(NOW)).not.toBeNull();
  setSwarmContinuousQuality(true);
  expect(loadSwarmBenchmark(NOW)).toBeNull();
 });
 it('invalidates failed, partially graded, altered or stale receipts',()=>{
  const partial=source();partial.results.pop();
  for(const bad of [
    {...receipt(),status:'blocked'},
    {...receipt(),source:'model_says_it_won'},
    {...receipt(),validUntil:new Date(NOW-1).toISOString()},
    {...receipt(),regressionTargets:['ollama:local-reviewed']},
    receipt(partial),
  ])expect(()=>validateContinuousQualityReceipt(bad,NOW)).toThrow();
  importSwarmBenchmark(source(),NOW,true);
  setSwarmContinuousQuality(true);
  invalidateSwarmContinuousQuality();
  expect(loadSwarmBenchmark(NOW)).toBeNull();
 });
});
