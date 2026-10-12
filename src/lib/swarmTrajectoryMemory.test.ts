import { describe, it, expect } from 'vitest';
import { buildSwarmTrajectoryContext } from './swarmTrajectoryMemory';
import type { ChatSession } from '../store/sessionStore';

function fixture(workspacePath='/workspace',status:'done'|'error'='done'):ChatSession {
  return {id:'session-a',workspacePath,workflowRunMeta:{
    'r1':{name:'private prompt',description:'vulnerable passwords secret 123',status,
      startedAt:10,finishedAt:20,
      spec:{name:'test',description:'audit backend API',phases:[{title:'T',
        agents:[{description:'A',prompt:'secret 999',profile:'explore'}]}]},
      phases:[{title:'T',agents:[{taskId:'a',description:'A'}]}],
      agentResults:{a:{promptHash:'12345678',status:'done',report:'SECRET_API_KEY=123'}}}
  }} as unknown as ChatSession;
}
describe('local read-only trajectory retrieval',()=>{
  it('summarizes actual execution without leaking text, paths, prompts or secrets',()=>{
    const block=buildSwarmTrajectoryContext([fixture()], 'backend','/workspace','session-x');
    expect(block).toContain('reported successful agents: 1');
    expect(block).toContain('NOT correctness evidence');
    expect(block).not.toMatch(/SECRET|private prompt|vulnerable|\\/workspace|secret 999/);
  });
  it('isolates workspace, domain and mutable agent execution',()=>{
    expect(buildSwarmTrajectoryContext([fixture()], 'security','/workspace','session-x')).toBe('');
    expect(buildSwarmTrajectoryContext([fixture()], 'backend','/other','session-x')).toBe('');
    const other=fixture();other.workflowRunMeta!.r1.spec!.phases[0].agents[0].profile='code';
    expect(buildSwarmTrajectoryContext([other], 'backend','/workspace','session-x')).toBe('');
  });
  it('does not cross untitled sessions or include active runs',()=>{
    expect(buildSwarmTrajectoryContext([fixture('')], 'backend','session:session-x','session-x')).toBe('');
    const entry=fixture();entry.workflowRunMeta!.r1.status='running';
    expect(buildSwarmTrajectoryContext([entry], 'backend','/workspace','session-x')).toBe('');
  });
});
