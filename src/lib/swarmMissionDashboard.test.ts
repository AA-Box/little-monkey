import {describe,it,expect} from 'vitest';
import {swarmMissionOverview,projectSwarmBudget} from './swarmMissionDashboard';
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
 it('reports unavailable measurements as unknown rather than zero',()=>{
  const session=sample();session.subagentRunMeta={};
  expect(swarmMissionOverview([session]).measuredTokens).toBeNull();
  expect(swarmMissionOverview([]).meanDurationMs).toBeNull();
 });
});
