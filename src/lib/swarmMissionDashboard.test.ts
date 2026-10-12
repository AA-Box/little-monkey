import {describe,it,expect} from 'vitest';
import {swarmMissionOverview,projectSwarmBudget,estimateSwarmMissionSpend} from './swarmMissionDashboard';
import type {ChatSession} from '../store/sessionStore';
function sample():ChatSession{
 return {id:'s',subagentRunMeta:{'a':{status:'done',description:'A',profile:'explore',
 startedAt:10,finishedAt:80,toolCallCount:0,usage:{promptTokens:90,completionTokens:10,totalTokens:100}}},
 workflowRunMeta:{'run-1':{name:'Read-only',description:'review',status:'done',startedAt:1,finishedAt:101,
 phases:[{title:'Survey',agents:[{taskId:'a',description:'A'},{taskId:'b',description:'B'}]}],
 agentResults:{'a':{status:'done',promptHash:'hash',report:'x'},
 'b':{status:'done',promptHash:'hash',report:'y',reused:true}}}},
 } as unknown as ChatSession;
}
describe('swarm observability from real saved journals',()=>{
 it('does not invent model costs or tokens from missing usage',()=>{
  const result=swarmMissionOverview([sample()]);
  expect(result.runs).toBe(1);expect(result.successful).toBe(1);
  expect(result.agentRuns).toBe(2);expect(result.measuredAgentRuns).toBe(1);
  expect(result.measuredTokens).toBe(100);expect(result.costUsd).toBeNull();
  expect(result.missions[0].reused).toBe(1);
  expect(result.reusedAgentRuns).toBe(1);
  expect(result.missions[0].promptTokens).toBe(90);
  expect(result.missions[0].completionTokens).toBe(10);
 });
 it('forecasts only when enough real measured samples exist',()=>{
  const x=swarmMissionOverview([sample()]);
  const unavailable=projectSwarmBudget(x,4,300);
  expect(unavailable.projectedTokens).toBeNull();
  expect(unavailable.overTokenBudget).toBeNull();
  const many=swarmMissionOverview([sample(),sample(),sample()]);
  const projection=projectSwarmBudget(many,4,300,2.5);
  expect(projection.projectedTokens).toBe(400);
  expect(projection.overTokenBudget).toBe(true);
  expect(projection.projectedUsd).toBe(0.001);
  expect(many.costUsd).toBeNull();
 });
 it('attributes only observed usage, excluding checkpoint replays and disclosing coverage',()=>{
   const measured=swarmMissionOverview([sample()]);
   const cost=estimateSwarmMissionSpend(measured,5);
   expect(cost.coverage).toBe(1);
   expect(cost.coveredAgentRuns).toBe(1);
   expect(cost.dispatchedAgentRuns).toBe(1);
   expect(cost.observedUsd).toBe(0.0005);
   expect(cost.perMission[0].estimatedUsd).toBe(0.0005);
   expect(()=>estimateSwarmMissionSpend(measured,-1)).toThrow(/nonnegative/);
   const noUsage=sample();noUsage.subagentRunMeta={};
   const partial=estimateSwarmMissionSpend(swarmMissionOverview([noUsage]),5);
   expect(partial.observedUsd).toBeNull();
   expect(partial.incomplete).toBe(true);
   expect(partial.perMission[0].estimatedUsd).toBeNull();
 });
 it('reports unavailable measurements as unknown rather than zero',()=>{
  const session=sample();session.subagentRunMeta={};
  expect(swarmMissionOverview([session]).measuredTokens).toBeNull();
  expect(swarmMissionOverview([]).meanDurationMs).toBeNull();
 });
});
