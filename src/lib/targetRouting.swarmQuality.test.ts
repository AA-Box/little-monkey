import { beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(), isTauri: () => false }));
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => {}) }));

import { routeFromActive } from './targetRouting';
import { useModelStore, type OllamaModelInfo } from '../store/modelStore';
import { useRoutingPolicyStore } from '../store/routingPolicyStore';
import {
  SWARM_BENCHMARK_KEY, SWARM_BENCHMARK_OPT_IN, setSwarmBenchmarkRouting,
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
function importFixtureRanking() {
  localStorage.setItem(SWARM_BENCHMARK_KEY,JSON.stringify({
    generatedAt:new Date().toISOString(),importedAt:Date.now(),
    ranking:[
      {target:'ollama:good-model',passes:3,attempts:3,accuracy:1,averageVerifiedMs:500},
      {target:'ollama:bad-model',passes:1,attempts:3,accuracy:0.3333,averageVerifiedMs:1000},
    ],
  }));
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
    localStorage.setItem(SWARM_BENCHMARK_KEY,JSON.stringify({
      generatedAt:new Date().toISOString(),importedAt:Date.now(),ranking:[
        {target:'ollama:good-model',passes:15,attempts:15,accuracy:1,averageVerifiedMs:500},
        {target:'ollama:bad-model',passes:15,attempts:15,accuracy:1,averageVerifiedMs:1000},
      ],
    }));
    setSwarmBenchmarkRouting(true);
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
    const optimized=routeFromActive(target,context);
    expect(optimized.target).toMatchObject({model:'bad-model'});
    expect(optimized.decision.reason).toContain('Opted-in cost optimizer');
    expect(optimized.decision.sequence.slice(0,2)).toEqual(['ollama:bad-model','ollama:good-model']);
    useRoutingPolicyStore.setState({policies:[{...policy,preferredTargetKeys:['ollama:good-model']}]});
    expect(routeFromActive(target,context).target).toMatchObject({model:'good-model'});
    useRoutingPolicyStore.setState({policies:[{...policy}]});
    expect(routeFromActive(target,{...context,taskClass:'chat'}).target).toMatchObject({model:'bad-model'});
  });

});
