/**
 * Continuous independent model-quality intake. The local resident benchmark
 * runner (not an LLM) writes the bounded receipt into the app data directory.
 * Only the primary desktop window reads it. A missing, stale, failed or
 * incomplete receipt immediately removes automatic quality authorization;
 * existing operator model-selection / privacy / permission gates still apply.
 */
import {isTauri} from '@tauri-apps/api/core';
import {appDataDir,join} from '@tauri-apps/api/path';
import {readTextFile} from '@tauri-apps/plugin-fs';
import {
 importSwarmBenchmark,invalidateSwarmContinuousQuality,parseLiveSwarmBenchmark,
 swarmContinuousQualityEnabled,type SwarmBenchmarkSummary,
} from './swarmVerifiedBenchmarks';
const HOURS_36=36*60*60_000;
export const CONTINUOUS_SWARM_RECEIPT_NAME='swarm-quality-latest.json';
export function validateContinuousQualityReceipt(
  raw:unknown,now=Date.now(),
):{report:unknown;summary:SwarmBenchmarkSummary}{
 if(!raw||typeof raw!=='object'||Array.isArray(raw))
  throw new Error('No continuous independent grade');
 const v=raw as Record<string,unknown>;
 if(v.version!==1||v.source!=='native_disposable_repository_independent_tests'||
    v.status!=='verified'||!Array.isArray(v.regressionTargets)||
    v.regressionTargets.length!==0||typeof v.createdAt!=='string'||
    typeof v.validUntil!=='string'){
  throw new Error('Continuous independent grading is missing or blocked');
 }
 const created=Date.parse(v.createdAt),expires=Date.parse(v.validUntil);
 if(!Number.isSafeInteger(created)||!Number.isSafeInteger(expires)||
    created>now+5*60_000||created<now-HOURS_36||
    expires<now||expires>created+HOURS_36+5000){
  throw new Error('Continuous benchmark expired');
 }
 const summary=parseLiveSwarmBenchmark(v.report,now);
 if(Math.abs(Date.parse(summary.generatedAt)-created)>300000)
  throw new Error('Continuous source fixture receipt does not match grading timestamp');
 if(summary.ranking.some(row=>row.attempts<9||row.accuracy<0.8))
  throw new Error('Real independent benchmark did not satisfy every target quality gate');
 return {report:v.report,summary};
}
export async function synchronizeContinuousSwarmQuality():Promise<SwarmBenchmarkSummary|null>{
 if(!swarmContinuousQualityEnabled())return null;
 try{
  if(!isTauri())throw new Error('Continuous quality needs a real desktop installation');
  const filename=await join(await appDataDir(),CONTINUOUS_SWARM_RECEIPT_NAME);
  const raw=await readTextFile(filename);
  if(raw.length>128000)throw new Error('Quality receipt is oversized');
  const {report}=validateContinuousQualityReceipt(JSON.parse(raw));
  return importSwarmBenchmark(report,Date.now(),true);
 }catch{
  invalidateSwarmContinuousQuality();
  return null;
 }
}
/** Called once at desktop bootstrap. Not bound to whether Settings is open. */
export function startContinuousSwarmQualityReader():()=>void{
 if(!isTauri()||typeof window==='undefined')return ()=>{};
 let inFlight=false;
 const refresh=()=>{
  if(inFlight||!swarmContinuousQualityEnabled())return;
  inFlight=true;
  void synchronizeContinuousSwarmQuality().finally(()=>{inFlight=false});
 };
 refresh();
 const timer=window.setInterval(refresh,60_000);
 window.addEventListener('focus',refresh);
 return ()=>{window.clearInterval(timer);window.removeEventListener('focus',refresh)};
}
