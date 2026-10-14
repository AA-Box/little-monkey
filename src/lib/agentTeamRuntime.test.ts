import {describe,expect,it,beforeEach} from 'vitest';
import {AgentTeamRuntime,MAX_RUNTIME_AGENTS,MAX_RUNTIME_DEPTH,AGENT_TEAM_TOOLS} from './agentTeamRuntime';
describe('live runtime agent collaboration',()=>{
  beforeEach(()=>localStorage.clear());
  it('creates children dynamically with depth, unique identity and concurrency budgets',()=>{
    const t=new AgentTeamRuntime('session/one');t.registerRoot('root');
    let parent='root';
    for(let i=0;i<MAX_RUNTIME_DEPTH;i++){const id='child-'+i;expect(t.reserveChild(parent,id)).toBe(i+1);parent=id;}
    expect(()=>t.reserveChild(parent,'too-deep')).toThrow(/depth/);
    expect(()=>t.reserveChild('root','child-0')).toThrow(/Duplicate/);
    expect(()=>t.reserveChild('unknown','x')).toThrow(/not active/);
    t.reserveChild('root','sixth-member');
    expect(()=>t.reserveChild('root','overflow')).toThrow(/Concurrent/);
    t.finish('child-3');
    expect(t.reserveChild('root','new-member')).toBe(1);
  });
  it('enforces total task budget even after children complete',()=>{
    const t=new AgentTeamRuntime('session/budget');t.registerRoot('root');
    for(let n=0;n<MAX_RUNTIME_AGENTS-1;n++){const id='child'+n;t.reserveChild('root',id);t.finish(id);}
    expect(()=>t.reserveChild('root','too-many')).toThrow(/count/);
  });
  it('delivers messages only to current peers and preserves bounded mail across reload',()=>{
    const t=new AgentTeamRuntime('session/persist');t.registerRoot('lead');t.reserveChild('lead','reviewer');
    const first=t.send('lead','reviewer','Review file x');
    expect(t.inbox('reviewer',0)).toEqual([first]);
    expect(t.inbox('lead',0)).toEqual([]);
    expect(t.inbox('reviewer',first.seq)).toEqual([]);
    expect(()=>t.send('outsider','reviewer','bad')).toThrow(/inactive/);
    expect(()=>t.send('lead','not-member','bad')).toThrow(/inactive/);
    expect(()=>t.send('lead','reviewer','x'.repeat(2001))).toThrow(/2000/);
    expect(t.peers('lead')).toEqual(['reviewer']);
    const t2=new AgentTeamRuntime('session/persist');t2.registerRoot('lead');t2.reserveChild('lead','reviewer');
    expect(t2.inbox('reviewer')).toEqual([first]);
  });
  it('join only accepts registered direct children',async()=>{
    const t=new AgentTeamRuntime('session/join');t.registerRoot('lead');t.reserveChild('lead','worker');
    t.attachCompletion('lead','worker',Promise.resolve('verified result'));
    await expect(t.join('lead','worker')).resolves.toBe('verified result');
    await expect(t.join('worker','lead')).rejects.toThrow(/direct/);
    t.finish('worker');expect(()=>t.send('lead','worker','late')).toThrow(/inactive/);
  });
  it('only advertises finite, strict tool contracts',()=>{
    expect(AGENT_TEAM_TOOLS.map(t=>t.function.name)).toEqual(['agent_spawn','agent_send','agent_inbox','agent_join']);
    for(const tool of AGENT_TEAM_TOOLS) expect((tool.function.parameters as {additionalProperties?:boolean}).additionalProperties).toBe(false);
  });
});