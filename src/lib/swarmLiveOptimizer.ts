/**
 * Continuous, event-driven *operational* feedback for opt-in swarm cost routing.
 *
 * Source: real subagent terminal outcomes, emitted once by the actual run
 * executor. "done" means the job finished, NOT that its answer was correct.
 * Correctness eligibility continues to require imported independent fixtures
 * in swarmCostOptimizer.ts. A candidate never leaves the already-admitted
 * routing-policy allowlist or overrides a pin/privacy gate.
 *
 * No prompts, content, paths, identities, or peer-provided success reports.
 */
export const SWARM_LIVE_OUTCOMES_KEY='little-monkey-swarm-live-outcomes-v1';
const MAX_RECORDS=600;
const WINDOW_MS=14*24*60*60_000;
const MAX_ELAPSED_MS=3*60*60_000;
const TARGET=/^(ollama|provider|local):[A-Za-z0-9_.:%/@+-]{1,160}$/;

export interface SwarmLiveOutcome {
 target:string;
 status:'done'|'error';
 elapsedMs:number;
 finishedAtMs:number;
 /** Observed provider/local usage, when the model emitted usable token metering. */
 usage?:{promptTokens:number;completionTokens:number};
 /** User-configured rate estimate, NEVER the provider's settled invoice. */
 meteredCostUsd?:number|null;
}
export interface SwarmLiveDecision {
 sequence:string[];
 changed:boolean;
 phase:'disabled'|'insufficient-baseline'|'canary'|'ramp'|'established'|'regression-blocked'|'unchanged';
 baselineSamples:number;
 candidateSamples:number;
}
function read(now:number):SwarmLiveOutcome[]{
 try {
  const parsed:unknown=JSON.parse(localStorage.getItem(SWARM_LIVE_OUTCOMES_KEY)??'[]');
  if(!Array.isArray(parsed))return [];
  return parsed.slice(-MAX_RECORDS).filter((raw):raw is SwarmLiveOutcome=>{
   if(!raw||typeof raw!=='object')return false;
   const r=raw as Partial<SwarmLiveOutcome>;
   return typeof r.target==='string'&&TARGET.test(r.target)&&
    (r.status==='done'||r.status==='error')&&
    Number.isSafeInteger(r.finishedAtMs)&&r.finishedAtMs!>=now-WINDOW_MS&&
    r.finishedAtMs!<=now+5000&&Number.isSafeInteger(r.elapsedMs)&&
    r.elapsedMs!>=0&&r.elapsedMs!<=MAX_ELAPSED_MS&&
    (r.usage===undefined||(
      Number.isSafeInteger(r.usage.promptTokens)&&r.usage.promptTokens>=0&&
      r.usage.promptTokens<=100_000_000&&
      Number.isSafeInteger(r.usage.completionTokens)&&r.usage.completionTokens>=0&&
      r.usage.completionTokens<=100_000_000))&&
    (r.meteredCostUsd===undefined||r.meteredCostUsd===null||
      (Number.isFinite(r.meteredCostUsd)&&r.meteredCostUsd>=0&&r.meteredCostUsd<=1_000_000));
  });
 }catch{return [];}
}
export function loadSwarmLiveOutcomes(now=Date.now()):SwarmLiveOutcome[]{
 return read(now);
}
export function recordSwarmLiveOutcome(
 target:string|null|undefined,status:'done'|'error'|'cancelled',
 elapsedMs:number,now=Date.now(),
 usage?:{promptTokens:number;completionTokens:number},
 meteredCostUsd?:number|null,
):void{
 // Human cancellation and aborted pre-routing runs must not poison a model.
 if(!target||!TARGET.test(target)||status==='cancelled'||
    !Number.isSafeInteger(now)||now<=0||
    !Number.isFinite(elapsedMs)||elapsedMs<0||elapsedMs>MAX_ELAPSED_MS)return;
 try {
  const rows=read(now);
  const validUsage=usage&&Number.isSafeInteger(usage.promptTokens)&&
    Number.isSafeInteger(usage.completionTokens)&&usage.promptTokens>=0&&
    usage.completionTokens>=0&&usage.promptTokens<=100_000_000&&
    usage.completionTokens<=100_000_000;
  const validCost=meteredCostUsd!==null&&meteredCostUsd!==undefined&&
    Number.isFinite(meteredCostUsd)&&meteredCostUsd>=0&&meteredCostUsd<=1_000_000;
  rows.push({target,status,elapsedMs:Math.round(elapsedMs),finishedAtMs:now,
    ...(validUsage?{usage:{promptTokens:usage.promptTokens,
      completionTokens:usage.completionTokens}}:{}),
    ...(validCost?{meteredCostUsd}:{}),
  });
  localStorage.setItem(SWARM_LIVE_OUTCOMES_KEY,JSON.stringify(rows.slice(-MAX_RECORDS)));
 }catch{ /* Observability is best effort; never abort an executing subagent. */ }
}
function p90(values:number[]):number|null{
 if(!values.length)return null;
 values.sort((a,b)=>a-b);
 return values[Math.ceil(values.length*.9)-1]??null;
}
export function selectLiveGuardedCostRoute(
 baselineSequence:readonly string[],proposedSequence:readonly string[],
 enabled:boolean,now=Date.now(),bucket=Math.floor(Math.random()*10),
 observations:readonly SwarmLiveOutcome[]=read(now),
):SwarmLiveDecision{
 const before=[...baselineSequence];
 const unchanged=(phase:SwarmLiveDecision['phase'],base=0,candidate=0):SwarmLiveDecision=>({
  sequence:before,changed:false,phase,baselineSamples:base,candidateSamples:candidate,
 });
 if(!enabled||before.length<2||proposedSequence.length!==before.length||
    !Number.isSafeInteger(now)||now<=0||
    !Number.isInteger(bucket)||bucket<0||bucket>9||
    new Set(before).size!==before.length||
    new Set(proposedSequence).size!==proposedSequence.length||
    proposedSequence.some(x=>!before.includes(x)))return unchanged('disabled');
 if(proposedSequence[0]===before[0])return unchanged('unchanged');
 const baseline=before[0],candidate=proposedSequence[0];
 const recent=observations.filter(row=>row.finishedAtMs>=now-WINDOW_MS&&
    row.finishedAtMs<=now+5000&&Number.isSafeInteger(row.elapsedMs)&&
    row.elapsedMs>=0&&row.elapsedMs<=MAX_ELAPSED_MS&&
    (row.status==='done'||row.status==='error'));
 const base=recent.filter(row=>row.target===baseline).slice(-100);
 const trial=recent.filter(row=>row.target===candidate).slice(-100);
 if(base.length<6)return unchanged('insufficient-baseline',base.length,trial.length);
 // This is a reliability circuit breaker, not a correctness score. Independently
 // verified benchmark equivalence is still mandatory upstream.
 const success=(xs:SwarmLiveOutcome[])=>xs.filter(x=>x.status==='done').length/xs.length;
 const baselineSuccess=success(base),candidateSuccess=trial.length?success(trial):1;
 const baseLatency=p90(base.filter(x=>x.status==='done').map(x=>x.elapsedMs));
 const trialLatency=p90(trial.filter(x=>x.status==='done').map(x=>x.elapsedMs));
 const rapidFailures=trial.slice(-3).filter(x=>x.status==='error').length>=2;
 if((trial.length>=6&&candidateSuccess+0.08<baselineSuccess)||
    (trial.length>=3&&rapidFailures&&baselineSuccess>=0.8)||
    (trial.length>=6&&baseLatency!==null&&trialLatency!==null&&
      trialLatency>Math.max(1000,baseLatency*2.5))){
  return unchanged('regression-blocked',base.length,trial.length);
 }
 // Progressive rollout so unseen models do not silently take 100% of calls.
 // Run-time bucket is chosen anew per dispatch; no background task required.
 const phase:SwarmLiveDecision['phase']=trial.length<6?'canary':trial.length<12?'ramp':'established';
 const percent=phase==='canary'?1:phase==='ramp'?3:10;
 if(bucket>=percent)return unchanged(phase,base.length,trial.length);
 return {sequence:[candidate,...before.filter(key=>key!==candidate)],
  changed:true,phase,baselineSamples:base.length,candidateSamples:trial.length};
}
