import { beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(), isTauri: () => false }));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => {}) }));

import { routeFromActive } from './targetRouting';
import { useModelStore, type OllamaModelInfo } from '../store/modelStore';
import { useRoutingPolicyStore } from '../store/routingPolicyStore';
import {
  SWARM_BENCHMARK_KEY, SWARM_BENCHMARK_OPT_IN, setSwarmBenchmarkRouting, importSwarmBenchmark,
} from './swarmVerifiedBenchmarks';
import type { ResolvedTarget } from './turnEngine';
import type { RoutingPolicy } from './modelRouting';
import {useCostControlStore} from '../store/costControlStore';
import {setSwarmCostOptimization,SWARM_COST_ROUTING_OPT_IN} from './swarmCostOptimizer';

const target: ResolvedTarget = { kind: 'ollama', baseUrl: 'http://127.0.0.1:11434', model: 'bad-model' };
const policy: RoutingPolicy = {
  id: 'quality-review',name:'Locally approved review',
  enabled:true,taskClasses:['subagent_explore','chat'],preferredTargetKeys:[],
  requiresTools:true,sensitivity:'any',
  maxInputPerMillionUsd:null,maxOutputPerMillionUsd:null,maxTimeToFirstTokenMs:null,
};
function model(name: string, cloud = false): OllamaModelInfo {
  return {name,size_bytes:2_000_000_000,is_cloud:cloud,tool_calling:true,vision:false,modified_at:'2026-10-12T00:00:00Z'};
}
const context = {taskClass:'subagent_explore' as const,requiresVision:false,requiresTools:true};
beforeEach(() => {
  // Vitest runs these state-machine tests in Node, not necessarily jsdom.
  // Use an isolated deterministic storage implementation per test.
  const memory = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => memory.get(key) ?? null,
    setItem: (key: string, value: string) => { memory.set(key, value); },
    removeItem: (key: string) => { memory.delete(key); },
  });
  localStorage.removeItem(SWARM_BENCHMARK_KEY);
  localStorage.removeItem(SWARM_BENCHMARK_OPT_IN);
  localStorage.removeItem(SWARM_COST_ROUTING_OPT_IN);
  useCostControlStore.setState({rates:{},entries:[]});
  useRoutingPolicyStore.setState({policies:[{...policy}],lastDecision:null});
  useModelStore.setState({
    installed:[],active:null,llamaStatus:'stopped',
    activeProvider:'ollama',activeOllamaModel:'bad-model',
    ollamaReachable:true,ollamaModels:[model('bad-model'),model('good-model')],
    providers:[],providerModels:{},mlxChat:null,
  });
});
function importFixtureRanking(equalAccuracy=false) {
  const targets=['ollama:good-model','ollama:bad-model'];
  const fixtures=['logic-regression','authorization-regression','parsing-regression'];
  const repeats=equalAccuracy?5:1;
  const results=targets.flatMap((target,index)=>Array.from({length:repeats},(_,r)=>
    fixtures.map((fixture,j)=>({
      target,fixture,repeat:r+1,runId:'run-bench-'+index+'-'+r+'-'+j,
      verified:equalAccuracy||index===0||j===0,
      durationMs:index===0?500:1000,
    }))).flat());
  const ranking=targets.map((target,index)=>({
    target,passes:index===0||equalAccuracy?repeats*3:repeats,
    attempts:repeats*3,
    accuracy:index===0||equalAccuracy?1:0.3333,
    averageVerifiedMs:index===0?500:1000,
  }));
  importSwarmBenchmark({
    version:1,benchmark:'live-task-verified-fixtures',mode:'real-model',
    generatedAt:new Date().toISOString(),ranking,results,
  });
  setSwarmBenchmarkRouting(true);
}
describe('benchmarked swarm model selection respects live policy constraints', () => {
  it('uses the active configured model when opt-in is absent', () => {
    expect(routeFromActive(target,context).target).toMatchObject({model:'bad-model'});
  });
  it('selects the measured high-accuracy model only among already eligible subagents', () => {
    importFixtureRanking();
    const result=routeFromActive(target,context);
    expect(result.target).toMatchObject({kind:'ollama',model:'good-model'});
    expect(result.decision.sequence.slice(0,2)).toEqual(['ollama:good-model','ollama:bad-model']);
    expect(result.decision.reason).toContain('Operator-approved live-model');
  });
  it('does not override explicit policy pins or globally selected chat models', () => {
    importFixtureRanking();
    useRoutingPolicyStore.setState({policies:[{...policy,preferredTargetKeys:['ollama:bad-model']}]});
    expect(routeFromActive(target,context).target).toMatchObject({model:'bad-model'});
    useRoutingPolicyStore.setState({policies:[{...policy}]});
    expect(routeFromActive(target,{...context,taskClass:'chat'}).target).toMatchObject({model:'bad-model'});
  });
  it('cannot route to a higher-scored model excluded by existing eligibility', () => {
    importFixtureRanking();
    useModelStore.setState({ollamaModels:[model('bad-model'),model('good-model',true)]});
    useRoutingPolicyStore.setState({policies:[{...policy,sensitivity:'local_only'}]});
    expect(routeFromActive(target,context).target).toMatchObject({model:'bad-model'});
  });
  it('end-to-end cost optimizer only reorders equally verified and previously eligible subagent models',()=>{
    importFixtureRanking(true);
    useCostControlStore.setState({
      rates:{'ollama:good-model':{inputPerMillionUsd:10,outputPerMillionUsd:20},
             'ollama:bad-model':{inputPerMillionUsd:1,outputPerMillionUsd:2}},
      entries:[0,1,2,3].map(i=>({id:'c'+i,occurredAtMs:Date.now(),targetKey:'ollama:good-model',
        targetLabel:'good-model',sessionId:'s',runId:null,
        usage:{promptTokens:400,completionTokens:100,totalTokens:500},costUsd:null})),
    });
    const quality=routeFromActive(target,context);
    expect(quality.target).toMatchObject({model:'good-model'});
    setSwarmCostOptimization(true);
    // A cost-only proposal no longer automatically captures all traffic.
    // Real terminal outcomes must first establish the primary's reliability.
    localStorage.setItem('little-monkey-swarm-live-outcomes-v1',JSON.stringify(
      Array.from({length:6},(_,i)=>({target:'ollama:good-model',status:'done',
        elapsedMs:800,finishedAtMs:Date.now()-i*1000}))));
    const canary=vi.spyOn(Math,'random').mockReturnValue(0);
    const optimized=routeFromActive(target,context);
    expect(optimized.target).toMatchObject({model:'bad-model'});
    expect(optimized.decision.reason).toContain('Opted-in cost optimizer');
    expect(optimized.decision.sequence.slice(0,2)).toEqual(['ollama:bad-model','ollama:good-model']);
    useRoutingPolicyStore.setState({policies:[{...policy,preferredTargetKeys:['ollama:good-model']}]});
    expect(routeFromActive(target,context).target).toMatchObject({model:'good-model'});
    useRoutingPolicyStore.setState({policies:[{...policy}]});
    expect(routeFromActive(target,{...context,taskClass:'chat'}).target).toMatchObject({model:'bad-model'});
    canary.mockRestore();
  });

});
