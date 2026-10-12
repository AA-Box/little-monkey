/** Read-only mission telemetry computed from durable session journals and
 * installed daemon graph status. Unknown usage and price are NEVER zero.
 * No prompts, reports, source text or secrets leave local session records. */
import type {ChatSession,WorkflowRunMeta} from '../store/sessionStore';
import type {ResidentSwarmGraphStatus} from './daemonClient';
export interface SwarmMissionRow {
 id:string;name:string;status:WorkflowRunMeta['status'];
 elapsedMs:number;agents:number;done:number;failed:number;reused:number;
 tokens:number|null;measuredAgents:number;domain:string;topology:string;
}
export interface SwarmMissionOverview {
 runs:number;successful:number;failed:number;cancelled:number;running:number;
 agentRuns:number;measuredAgentRuns:number;
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
   let done=0,failed=0,reused=0,measuredAgents=0,tokenSum=0;
   for(const phase of run.phases)for(const agent of phase.agents){
    const journal=run.agentResults?.[agent.taskId];
    if(journal?.status==='done')done++;
    if(journal?.status==='error')failed++;
    if(journal?.reused)reused++;
    const usage=session.subagentRunMeta?.[agent.taskId]?.usage;
    if(usage && Number.isSafeInteger(usage.totalTokens) && usage.totalTokens>=0){
      measuredAgents++;tokenSum+=usage.totalTokens;
    }
   }
   rows.push({
    id,name:run.name.slice(0,100),status:run.status,
    elapsedMs:Math.max(0,run.finishedAt-run.startedAt),
    agents,done,failed,reused,
    tokens:measuredAgents?tokenSum:null,measuredAgents,
    domain:run.spec?.name.startsWith('auto-swarm-')?run.spec.description.slice(0,60):'custom',
    topology:run.spec?.topology??'phased',
   });
  }
 }
 rows.sort((a,b)=>b.elapsedMs-a.elapsedMs || a.name.localeCompare(b.name));
 const terminal=rows.filter(r=>r.status!=='running');
 const measured=rows.filter(r=>r.tokens!==null);
 const states=resident.map(g=>g.state);
 return {
  runs:rows.length,successful:rows.filter(r=>r.status==='done').length,
  failed:rows.filter(r=>r.status==='error').length,
  cancelled:rows.filter(r=>r.status==='cancelled').length,
  running:rows.filter(r=>r.status==='running').length,
  agentRuns:rows.reduce((n,r)=>n+r.agents,0),
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
