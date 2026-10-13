/** Read-only mission telemetry computed from durable session journals and
 * installed daemon graph status. Unknown usage and price are NEVER zero.
 * No prompts, reports, source text or secrets leave local session records. */
import type {ChatSession,WorkflowRunMeta} from '../store/sessionStore';
import type {ResidentSwarmGraphStatus} from './daemonClient';
import type {LabCostRate} from './compareLab';
import {calculateUsageCostUsd} from '../store/costControlStore';
export interface SwarmMissionRow {
 id:string;name:string;status:WorkflowRunMeta['status'];
 startedAt:number;elapsedMs:number;agents:number;done:number;failed:number;cancelled:number;reused:number;
 tokens:number|null;promptTokens:number|null;completionTokens:number|null;measuredAgents:number;domain:string;topology:string;
 routing: {agent:string;targetLabel:string;targetKey?:string;reason:string;policyName:string|null}[];
}
export interface SwarmMissionOverview {
 runs:number;successful:number;failed:number;cancelled:number;running:number;
 agentRuns:number;measuredAgentRuns:number;reusedAgentRuns:number;failedAgentRuns:number;cancelledAgentRuns:number;
 measuredTokens:number|null;costUsd:null;
 meanDurationMs:number|null;missions:SwarmMissionRow[];
 resident:{running:number;succeeded:number;failed:number;cancelled:number;nodes:number};
}
export function swarmMissionOverview(
 sessions:readonly ChatSession[],resident:readonly ResidentSwarmGraphStatus[]=[],
):SwarmMissionOverview {
 const rows:SwarmMissionRow[]=[];
 for(const session of sessions){
  for(const [id,run] of Object.entries(session.workflowRunMeta??{})){
   const agents=run.phases.reduce((sum,phase)=>sum+phase.agents.length,0);
   if(!agents)continue;
   let done=0,failed=0,cancelled=0,reused=0,measuredAgents=0,tokenSum=0,promptSum=0,completionSum=0;
   const routing: SwarmMissionRow['routing'] = [];
   for(const phase of run.phases)for(const agent of phase.agents){
    const journal=run.agentResults?.[agent.taskId];
    if(journal?.status==='done')done++;
    if(journal?.status==='error')failed++;
    if(journal?.status==='cancelled')cancelled++;
    if(journal?.reused)reused++;
    const agentMeta=session.subagentRunMeta?.[agent.taskId];
    if(agentMeta?.routing && routing.length<16)routing.push({
      agent:agent.description.slice(0,120),...agentMeta.routing,
    });
    const usage=agentMeta?.usage;
    if(usage && Number.isSafeInteger(usage.totalTokens) && usage.totalTokens>=0){
      measuredAgents++;tokenSum+=usage.totalTokens;
      if(Number.isSafeInteger(usage.promptTokens) && usage.promptTokens>=0)promptSum+=usage.promptTokens;
      if(Number.isSafeInteger(usage.completionTokens) && usage.completionTokens>=0)completionSum+=usage.completionTokens;
    }
   }
   rows.push({
    id:session.id+':'+id,name:run.name.slice(0,100),status:run.status,
    startedAt:run.startedAt,elapsedMs:Math.max(0,run.finishedAt-run.startedAt),
    agents,done,failed,cancelled,reused,
    tokens:measuredAgents?tokenSum:null,
    promptTokens:measuredAgents?promptSum:null,
    completionTokens:measuredAgents?completionSum:null,measuredAgents,routing,
    domain:run.spec?.name.startsWith('auto-swarm-')?run.spec.description.slice(0,60):'custom',
    topology:run.spec?.topology??'phased',
   });
  }
 }
 rows.sort((a,b)=>b.startedAt-a.startedAt || a.name.localeCompare(b.name));
 const terminal=rows.filter(r=>r.status!=='running');
 const measured=rows.filter(r=>r.tokens!==null);
 const states=resident.map(g=>g.state);
 return {
  runs:rows.length,successful:rows.filter(r=>r.status==='done').length,
  failed:rows.filter(r=>r.status==='error').length,
  cancelled:rows.filter(r=>r.status==='cancelled').length,
  running:rows.filter(r=>r.status==='running').length,
  agentRuns:rows.reduce((n,r)=>n+r.agents,0),
  reusedAgentRuns:rows.reduce((n,r)=>n+r.reused,0),
  failedAgentRuns:rows.reduce((n,r)=>n+r.failed,0),
  cancelledAgentRuns:rows.reduce((n,r)=>n+r.cancelled,0),
  measuredAgentRuns:rows.reduce((n,r)=>n+r.measuredAgents,0),
  measuredTokens:measured.length?measured.reduce((n,r)=>n+(r.tokens??0),0):null,
  costUsd:null, // requires a verified per-model rate AND provider billing receipt
  meanDurationMs:terminal.length?Math.round(terminal.reduce((n,r)=>n+r.elapsedMs,0)/terminal.length):null,
  missions:rows.slice(0,30),
  resident:{
    running:states.filter(x=>x==='running').length,
    succeeded:states.filter(x=>x==='succeeded').length,
    failed:states.filter(x=>x==='failed'||x==='failing').length,
    cancelled:states.filter(x=>x==='cancelled').length,
    nodes:resident.reduce((n,g)=>n+g.tasks.length,0),
  },
 };
}

/** An explicitly labelled projection, not measured spend. With fewer than
 * three observed agent usage records no forecast is reported. */
export interface SwarmBudgetProjection {
 measuredTokens:number|null;
 projectedTokens:number|null;
 projectedUsd:number|null;
 overTokenBudget:boolean|null;
 sampleAgents:number;
 coverage:number;
}
export function projectSwarmBudget(
 overview:SwarmMissionOverview, plannedAgents:number,
 tokenBudget:number|null=null,usdPerMillionTokens:number|null=null,
):SwarmBudgetProjection{
 if(!Number.isSafeInteger(plannedAgents)||plannedAgents<1||plannedAgents>16)
   throw new Error('A budget forecast needs 1–16 planned agents.');
 if(tokenBudget!==null && (!Number.isSafeInteger(tokenBudget)||tokenBudget<1))
   throw new Error('Token budget must be a positive integer.');
 if(usdPerMillionTokens!==null && (!Number.isFinite(usdPerMillionTokens)||usdPerMillionTokens<0))
   throw new Error('Model price estimate must be nonnegative and finite.');
 const sampleAgents=overview.measuredAgentRuns;
 const available=sampleAgents>=3&&overview.measuredTokens!==null;
 const projectedTokens=available?Math.ceil(overview.measuredTokens!/sampleAgents*plannedAgents):null;
 return {
  measuredTokens:overview.measuredTokens,projectedTokens,
  projectedUsd:projectedTokens!==null&&usdPerMillionTokens!==null
    ? Number((projectedTokens/1_000_000*usdPerMillionTokens).toFixed(4)):null,
  overTokenBudget:projectedTokens!==null&&tokenBudget!==null?projectedTokens>tokenBudget:null,
  sampleAgents,coverage:overview.agentRuns?sampleAgents/overview.agentRuns:0,
 };
}


/** Observed token-cost estimate from an operator-provided uniform unit rate.
 * This is intentionally not a provider bill or per-model attribution:
 * model identity and billed cached-token classes are not available in the
 * workflow journals. A partial number always carries explicit coverage.
 * Replayed checkpoint agents are not invoiced again here. */
export interface SwarmMissionSpend {
  observedUsd:number|null;
  coveredAgentRuns:number;
  dispatchedAgentRuns:number;
  coverage:number|null;
  incomplete:boolean;
  perMission:{id:string;estimatedUsd:number|null;covered:number;dispatched:number;failed:number;cancelled:number}[];
}
export function estimateSwarmMissionSpend(
  overview:SwarmMissionOverview, usdPerMillionTokens:number|null,
):SwarmMissionSpend {
  if(usdPerMillionTokens!==null && (!Number.isFinite(usdPerMillionTokens) || usdPerMillionTokens<0))
    throw new Error('Estimated price must be a finite nonnegative number.');
  const dispatchedAgentRuns=Math.max(0,overview.agentRuns-overview.reusedAgentRuns);
  const rows=overview.missions.map(row=>{
    const dispatched=Math.max(0,row.agents-row.reused);
    return {id:row.id,estimatedUsd:usdPerMillionTokens!==null
      ? row.tokens!==null ? Number((row.tokens*usdPerMillionTokens/1_000_000).toFixed(6))
        : dispatched===0 ? 0 : null
      : null,
      covered:row.measuredAgents,dispatched,failed:row.failed,cancelled:row.cancelled};
  });
  return {
    observedUsd:usdPerMillionTokens!==null && overview.measuredTokens!==null
      ? Number((overview.measuredTokens*usdPerMillionTokens/1_000_000).toFixed(6))
      : dispatchedAgentRuns===0 && usdPerMillionTokens!==null ? 0 : null,
    coveredAgentRuns:overview.measuredAgentRuns,dispatchedAgentRuns,
    coverage:dispatchedAgentRuns?Math.min(1,overview.measuredAgentRuns/dispatchedAgentRuns):null,
    incomplete:overview.measuredAgentRuns<dispatchedAgentRuns,
    perMission:rows,
  };
}


/**
 * Per-model estimated spend, joined by the original deterministic agent task
 * id and the target that the subagent actually dispatched. These records
 * carry usage observations, not invoices; provider-side discounts, cache
 * pricing and billing reconciliation are deliberately not inferred.
 */
export interface SwarmRatedSpend {
 estimatedUsd:number|null;
 coveredAgents:number;
 dispatchedAgents:number;
 measuredUnpricedAgents:number;
 unmeasuredAgents:number;
 incomplete:boolean;
 missions:{id:string;estimatedUsd:number|null;covered:number;dispatched:number}[];
}
export function estimateSwarmRatedSpend(
 sessions:readonly ChatSession[],
 rates:Readonly<Record<string,LabCostRate>>,
):SwarmRatedSpend {
 const missions:SwarmRatedSpend['missions']=[];
 let total=0,covered=0,dispatched=0,measuredUnpriced=0,unmeasured=0;
 for(const session of sessions){
  for(const [runId,run] of Object.entries(session.workflowRunMeta??{})){
   let missionTotal=0,missionCovered=0,missionDispatched=0;
   for(const phase of run.phases)for(const agent of phase.agents){
    // A replay did not make a new model request and must not be billed twice.
    if(run.agentResults?.[agent.taskId]?.reused)continue;
    dispatched++;missionDispatched++;
    const meta=session.subagentRunMeta?.[agent.taskId],usage=meta?.usage;
    if(!usage || !Number.isSafeInteger(usage.promptTokens)||usage.promptTokens<0 ||
       !Number.isSafeInteger(usage.completionTokens)||usage.completionTokens<0 ||
       !Number.isSafeInteger(usage.totalTokens)||usage.totalTokens<0){
      unmeasured++;continue;
    }
    const key=meta?.routing?.targetKey;
    const cost=key ? calculateUsageCostUsd(rates[key],usage) : null;
    if(cost===null){measuredUnpriced++;continue;}
    covered++;missionCovered++;total+=cost;missionTotal+=cost;
   }
   if(missionDispatched)missions.push({
     id:session.id+':'+runId,
     estimatedUsd:missionCovered?Number(missionTotal.toFixed(6)):null,
     covered:missionCovered,dispatched:missionDispatched,
   });
  }
 }
 return {estimatedUsd:covered?Number(total.toFixed(6)):null,
  coveredAgents:covered,dispatchedAgents:dispatched,
  measuredUnpricedAgents:measuredUnpriced,unmeasuredAgents:unmeasured,
  incomplete:covered<dispatched,missions};
}
