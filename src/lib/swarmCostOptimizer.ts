/**
 * Cost-aware, accuracy-constrained subagent routing.
 *
 * This only reorders the candidate list already admitted by the existing
 * routing policy. Never creates a model target, overrides an explicit pin,
 * substitutes self-reported agent success for independently graded fixtures,
 * or assumes rates / observed usage that have not been recorded.
 */
import type { LabCostRate } from './compareLab';
import type { CostUsageEntry } from '../store/costControlStore';
import type { SwarmBenchmarkSummary } from './swarmVerifiedBenchmarks';

export const SWARM_COST_ROUTING_OPT_IN = 'little-monkey-swarm-cost-routing-v1';
export function swarmCostOptimizationEnabled():boolean{
  try{return localStorage.getItem(SWARM_COST_ROUTING_OPT_IN)==='true';}
  catch{return false;}
}
export function setSwarmCostOptimization(enabled:boolean):void{
  localStorage.setItem(SWARM_COST_ROUTING_OPT_IN,enabled?'true':'false');
}
export interface SwarmCostRoute {
  sequence:string[];
  changed:boolean;
  estimatedSavingsFraction:number|null;
  evidenceSamples:number;
}
export function optimizeSwarmRoutingCosts(
  eligibleKeys:readonly string[],
  ranking:SwarmBenchmarkSummary|null,
  rates:Readonly<Record<string,LabCostRate>>,
  usage:readonly CostUsageEntry[],
  enabled:boolean,
  preferredKeys:readonly string[]=[],
  now=Date.now(),
):SwarmCostRoute {
  const original=[...eligibleKeys];
  const unchanged=(samples=0):SwarmCostRoute=>({
    sequence:original,changed:false,estimatedSavingsFraction:null,evidenceSamples:samples,
  });
  if(!enabled||preferredKeys.length||!ranking||original.length<2||
     new Set(original).size!==original.length)return unchanged();
  const scores=new Map(ranking.ranking
    .filter(row=>Number.isInteger(row.attempts)&&row.attempts>=6&&
      row.accuracy>=0.8&&row.accuracy<=1)
    .map(row=>[row.target,row]));
  const baseline=scores.get(original[0]);
  if(!baseline)return unchanged();
  // Use a SHARED observed input/output mix for all targets: comparing each
  // provider's own outputs biases the estimate by different answer lengths.
  // A routing price estimate is only useful for a recent workload mix.
  // Ignore old, malformed and future-dated usage rather than allowing a
  // stale token distribution to change a newly configured model choice.
  if(!Number.isFinite(now)||now<=0)return unchanged();
  const windowStart=now-14*24*60*60_000;
  const samples=usage.filter(row=>original.includes(row.targetKey) &&
    Number.isSafeInteger(row.occurredAtMs)&&
    row.occurredAtMs>=windowStart && row.occurredAtMs<=now+5*60_000 &&
    Number.isFinite(row.usage?.promptTokens)&&row.usage.promptTokens>=0 &&
    Number.isFinite(row.usage?.completionTokens)&&row.usage.completionTokens>=0 &&
    row.usage.promptTokens+row.usage.completionTokens>0).slice(-100);
  if(samples.length<3)return unchanged(samples.length);
  const prompt=samples.reduce((total,item)=>total+item.usage.promptTokens,0);
  const completion=samples.reduce((total,item)=>total+item.usage.completionTokens,0);
  if(prompt+completion<=0||!Number.isFinite(prompt+completion))return unchanged(samples.length);
  const price=(key:string):number|null=>{
    const rate=rates[key];
    if(!rate||!Number.isFinite(rate.inputPerMillionUsd)||
       !Number.isFinite(rate.outputPerMillionUsd)||
       rate.inputPerMillionUsd<0||rate.outputPerMillionUsd<0)return null;
    const value=prompt*rate.inputPerMillionUsd+completion*rate.outputPerMillionUsd;
    return Number.isFinite(value)?value:null;
  };
  const baselinePrice=price(original[0]);
  if(baselinePrice===null||baselinePrice<=0)return unchanged(samples.length);
  const equivalents=original.filter(key=>{
    const quality=scores.get(key);
    // A cheaper model whose independently measured successful fixture runs
    // are more than 3x slower is not an equivalent substitution. Unknown
    // timings are not permission to bypass this service-quality guard.
    const latencyAcceptable=quality && baseline.averageVerifiedMs!==null &&
      quality.averageVerifiedMs!==null &&
      Number.isFinite(quality.averageVerifiedMs)&&
      Number.isFinite(baseline.averageVerifiedMs)&&
      quality.averageVerifiedMs<=Math.max(100,baseline.averageVerifiedMs*3);
    return quality && baseline.accuracy-quality.accuracy<=0.02 &&
      quality.accuracy>=0.8 && latencyAcceptable && price(key)!==null;
  });
  const leastExpensive=equivalents.sort((a,b)=>price(a)!-price(b)! ||
    original.indexOf(a)-original.indexOf(b))[0];
  if(!leastExpensive||leastExpensive===original[0])return unchanged(samples.length);
  const cheaper=price(leastExpensive)!;
  const savings=(baselinePrice-cheaper)/baselinePrice;
  // Avoid switch churn for trivial rate differences or uncertain mixtures.
  if(savings<0.1)return unchanged(samples.length);
  return {sequence:[leastExpensive,...original.filter(key=>key!==leastExpensive)],
    changed:true,estimatedSavingsFraction:savings,evidenceSamples:samples.length};
}
