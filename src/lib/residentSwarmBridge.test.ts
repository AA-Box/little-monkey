import { describe, expect, it } from 'vitest';
import { prepareResidentReadOnlySwarm } from './residentSwarmBridge';
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
    expect(result.caveat).toContain('not automatically injected');
  });
  it('rejects remote peers, write agents, isolation and browser-only secrets before any file write', () => {
    for (const bad of [
      { profile: 'code' },
      { peer: 'paired-node' },
      { isolation: 'worktree' },
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
