import { describe, expect, it } from 'vitest';
import { prepareResidentReadOnlySwarm, prepareResidentSwarm } from './residentSwarmBridge';
import type { WorkflowSpec } from './workflow';

const base = {
  version: 1,
  name: 'operator-selected-model',
  target: { ollama: 'qwen3:8b' },
  workspace: '/tmp/test-workspace',
  permission_mode: 'manual',
  prompt: 'Original base recipe prompt is not inherited',
  max_iterations: 15,
  timeout_seconds: 1200,
};
const spec: WorkflowSpec = {
  name: 'Review auth',
  description: 'Research',
  phases: [
    { title: 'Inspect', agents: [
      { description: 'Auth', prompt: 'Trace auth paths', profile: 'explore' },
      { description: 'Validation', prompt: 'Trace inputs', profile: 'explore' },
    ] },
    { title: 'Review', agents: [
      { description: 'Independent check', prompt: 'Independently verify source files', profile: 'explore' },
    ] },
  ],
};
describe('safe read-only workflow to resident daemon handoff', () => {
  it('retains DAG phase barriers, freezes a real model and always downgrades to plan permissions', () => {
    const result = prepareResidentReadOnlySwarm(spec, base, '/tmp/resident-plans', 'swarm-abc123');
    expect(result.graph.id).toBe('swarm-abc123');
    expect(result.graph.nodes).toHaveLength(3);
    expect(result.graph.max_parallel).toBe(2);
    expect(result.graph.nodes[0].depends_on).toEqual([]);
    expect(result.graph.nodes[1].depends_on).toEqual([]);
    expect(result.graph.nodes[2].depends_on).toEqual(['p0a0','p0a1']);
    expect(result.graph.nodes[0].include_dependency_reports).toBe(false);
    expect(result.graph.nodes[2].include_dependency_reports).toBe(true);
    expect(result.graphPath).toBe('/tmp/resident-plans/swarm-abc123-graph.json');
    const recipe = JSON.parse(result.recipes[0].contents);
    expect(recipe.target).toEqual({ ollama: 'qwen3:8b' });
    expect(recipe.workspace).toBe('/tmp/test-workspace');
    expect(recipe.permission_mode).toBe('plan');
    expect(recipe.max_iterations).toBe(15);
    expect(recipe.timeout_seconds).toBe(1200);
    expect(recipe.prompt).toBe('Trace auth paths');
    expect(recipe).not.toHaveProperty('channel_send');
    expect(recipe).not.toHaveProperty('placed_run');
    expect(result.caveat).toContain('chain-verified local model reports');
  });
  it('generates a truly separate preapproved PLAN model fallback for independent roots only',()=>{
    const alternate={...base,permission_mode:'plan',target:{ollama:'qwen3:14b'}};
    const generated=prepareResidentSwarm(spec,base,'/tmp/fallback','swarm-fallback','analyze',alternate);
    expect(generated.graph.nodes[0].fallback_recipe).toBe('/tmp/fallback/swarm-fallback-p0a0-fallback.json');
    expect(generated.graph.nodes[1].fallback_recipe).toBeDefined();
    expect(generated.graph.nodes[2].fallback_recipe).toBeUndefined();
    expect(generated.recipes).toHaveLength(5);
    const prepared=JSON.parse(generated.recipes.find(r=>r.path===generated.graph.nodes[0].fallback_recipe)!.contents);
    expect(prepared.permission_mode).toBe('plan');
    expect(prepared.target).toEqual({ollama:'qwen3:14b'});
    expect(prepared.prompt).toBe('Trace auth paths');
    expect(()=>prepareResidentSwarm(spec,base,'/tmp','swarm-x','implement',alternate)).toThrow(/Fallback/);
    expect(()=>prepareResidentSwarm(spec,base,'/tmp','swarm-x','analyze',{...alternate,workspace:'/different'})).toThrow(/identical workspace/);
    expect(()=>prepareResidentSwarm(spec,base,'/tmp','swarm-x','analyze',{...alternate,permission_mode:'manual'})).toThrow(/PLAN/);
    expect(()=>prepareResidentSwarm(spec,base,'/tmp','swarm-x','analyze',{...alternate,target:base.target})).toThrow(/different configured model/);
  });
  it('rejects remote peers, write agents, isolation and browser-only secrets before any file write', () => {
    for (const bad of [
      { profile: 'code' },
      { peer: 'paired-node' },
      { isolation: 'worktree' as const },
      { peerCandidates: ['paired-node'] },
    ]) {
      const changed: WorkflowSpec = { ...spec, phases: [{
        title: 'P', agents: [{ ...spec.phases[0].agents[0], ...bad }],
      }] };
      expect(() => prepareResidentReadOnlySwarm(changed, base, '/tmp/resident-plans', 'swarm-good')).toThrow(/read-only/);
    }
    expect(() => prepareResidentReadOnlySwarm({...spec, notebook:true},base,'/tmp','swarm-good')).toThrow(/Remove/);
    expect(() => prepareResidentReadOnlySwarm({...spec, memoryStackIds:['s']},base,'/tmp','swarm-good')).toThrow(/Remove/);
  });
  it('rejects unsafe model templates and output paths', () => {
    expect(() => prepareResidentReadOnlySwarm(spec,{...base,permission_mode:'bypass'},'/tmp','swarm-good')).toThrow(/manual or plan/);
    expect(() => prepareResidentReadOnlySwarm(spec,{...base,channel_send:{enabled:true}},'/tmp','swarm-good')).toThrow(/ordinary/);
    expect(() => prepareResidentReadOnlySwarm(spec,{...base,workspace:'relative'},'/tmp','swarm-good')).toThrow(/absolute/);
    expect(() => prepareResidentReadOnlySwarm(spec,{...base,target:{ollama:'local',api_key:'secret'}},'/tmp','swarm-good')).toThrow(/unsupported/);
    expect(() => prepareResidentReadOnlySwarm(spec,base,'./relative','swarm-good')).toThrow(/absolute/);
    expect(() => prepareResidentReadOnlySwarm(spec,base,'/tmp','../../escape')).toThrow(/graph id/);
  });
  it('refuses unresolved recipe placeholders and oversized model-generated plans', () => {
    const changed: WorkflowSpec = {...spec,phases:[{title:'P',agents:[
      {...spec.phases[0].agents[0],prompt:'Read {{secret}}' },
    ]}]};
    expect(() => prepareResidentReadOnlySwarm(changed,base,'/tmp','swarm-good')).toThrow(/no unresolved/);
    const tooMany: WorkflowSpec = {...spec,phases:Array.from({length:6},(_,i)=>({
      title:'P'+i,agents:Array.from({length:3},(_,j)=>({
        description:'d'+j,prompt:'p',profile:'explore',
      })),
    }))};
    expect(() => prepareResidentReadOnlySwarm(tooMany,base,'/tmp','swarm-good')).toThrow(/16 agents/);
  });
  it('emits Windows-safe paths when an absolute local Windows directory is selected', () => {
    const result=prepareResidentReadOnlySwarm({...spec,phases:[spec.phases[0]]},{...base,workspace:'C:\\work'},'C:\\jobs','swarm-abc');
    expect(result.graphPath).toBe('C:\\jobs\\swarm-abc-graph.json');
    expect(result.graph.nodes[0].recipe).toBe('C:\\jobs\\swarm-abc-p0a0.json');
  });
});

describe('explicitly approved resident mutation mode', () => {
  const implementation: WorkflowSpec = {
    ...spec, stopOnFailure: true,
    phases: [
      ...spec.phases,
      { title: 'Implement', agents: [{ description: 'Sole code worker', prompt: 'Fix verified issue', profile: 'code' }] },
      { title: 'Verify', agents: [{ description: 'Test runner', prompt: 'Run and verify tests', profile: 'code' }] },
    ],
  };
  it('preserves manual tool approvals, run-level dependency evidence, and sequential mutation', () => {
    const frozen = prepareResidentSwarm(implementation, base, '/tmp/approved', 'swarm-implement', 'implement');
    expect(frozen.graph.max_parallel).toBe(1);
    expect(frozen.graph.nodes[2].depends_on).toEqual(['p0a0','p0a1']);
    expect(frozen.graph.nodes[3].include_dependency_reports).toBe(true);
    expect(frozen.graph.nodes[4].depends_on).toEqual(['p2a0']);
    expect(JSON.parse(frozen.recipes[2].contents).permission_mode).toBe('plan');
    const writer = JSON.parse(frozen.recipes[3].contents);
    expect(writer.permission_mode).toBe('manual');
    expect(writer.system).toContain('Request normal permission');
    expect(frozen.caveat).toContain('reconciliation');
    expect(writer).not.toHaveProperty('allow_push');
  });
  it('refuses write execution without an existing operator-selected manual recipe', () => {
    expect(() => prepareResidentSwarm(implementation,{...base,permission_mode:'plan'},'/tmp','swarm-write','implement'))
      .toThrow(/manual-permission/);
  });
  it('refuses parallel modifying agents and remote/code worktree privilege escalation', () => {
    const twoWriters: WorkflowSpec = {...implementation, phases:[{
      title:'Concurrent write', agents:[
        {description:'A',prompt:'Change A',profile:'code'},
        {description:'B',prompt:'Change B',profile:'code'},
      ],
    }]};
    expect(() => prepareResidentSwarm(twoWriters,base,'/tmp','swarm-write','implement'))
      .toThrow(/parallel code agents/);
    const remote: WorkflowSpec = {...implementation, phases:[{
      title:'Code peer',agents:[{description:'A',prompt:'Change A',profile:'code',peer:'paired-node'}],
    }]};
    expect(() => prepareResidentSwarm(remote,base,'/tmp','swarm-write','implement'))
      .toThrow(/only for local/);
  });
});
