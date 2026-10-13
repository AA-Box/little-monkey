import { describe, expect, it } from 'vitest';
import { compileSwarmTaskGraph } from './swarmGraph';
import { MAX_WORKFLOW_AGENTS, MAX_WORKFLOW_PHASES } from './workflow';

const graphTask = (id: string, deps: string[] = []) => ({
  id, description: 'Task ' + id, prompt: 'Do task ' + id, profile: 'explore', depends_on: deps,
  it('executes parent before descendants with three-level bounded delegation', () => {
    const spec = compileSwarmTaskGraph({
      name: 'delegated-project',
      tasks: [
        { ...graphTask('review'), children: [
          { ...graphTask('frontend'), children: [
            graphTask('accessibility'),
          ] },
          graphTask('backend'),
        ] },
        graphTask('independent'),
      ],
    });
    expect(spec.phases[0].agents.map(a => a.description)).toEqual(['Task review', 'Task independent']);
    expect(spec.phases[1].agents.map(a => a.description)).toEqual(['Task frontend', 'Task backend']);
    expect(spec.phases[2].agents.map(a => a.description)).toEqual(['Task accessibility']);
    expect(spec.phases[1].agents[0].prompt).toContain('Prerequisites: review');
    expect(spec.phases[2].agents[0].prompt).toContain('Prerequisites: frontend');
    expect(spec.stopOnFailure).toBe(true);
  });

  it('rejects nested cycles, duplicate identifiers, oversized trees, unknown children and excess depth', () => {
    const nested = (...children: unknown[]) => ({ ...graphTask('parent'), children });
    expect(() => compileSwarmTaskGraph({ name:'dup', tasks:[nested(graphTask('parent'))] })).toThrow(/unique/);
    expect(() => compileSwarmTaskGraph({ name:'cycle', tasks:[nested(graphTask('child',['parent'])), { ...graphTask('independent',['child']), depends_on:['child','parent'] }, { ...graphTask('child'), depends_on:['independent'] }] })).toThrow(/unique/);
    expect(() => compileSwarmTaskGraph({ name:'many',tasks:[nested(...Array.from({length:7},(_,i)=>graphTask('worker'+i)))] })).toThrow(/at most six/);
    expect(() => compileSwarmTaskGraph({ name:'depth',tasks:[
      { ...graphTask('a'),children:[{ ...graphTask('b'),children:[{ ...graphTask('c'),children:[graphTask('d')] }] }] },
    ] })).toThrow(/three levels/);
    expect(() => compileSwarmTaskGraph({ name:'too-many',tasks:[
      { ...graphTask('a'),children:Array.from({length:6},(_,i)=>({ ...graphTask('child'+i),children:Array.from({length:2},(_,j)=>graphTask('grand'+i+'x'+j)) })) },
    ] })).toThrow(/16 agents/);
    expect(() => compileSwarmTaskGraph({ name:'unknown',tasks:[{ ...graphTask('a'),children:'bad' }] })).toThrow(/children/);
    expect(() => compileSwarmTaskGraph({ name:'bad',tasks:[{...graphTask('a'),tools:['shell']}] })).toThrow(/Unknown graph task fields/);
  });

});

describe('dependency-aware native workflow graph', () => {
  it('orders prerequisites and parallel independent nodes into bounded waves', () => {
    const spec = compileSwarmTaskGraph({
      name: 'api-change',
      description: 'Change API safely',
      tasks: [graphTask('b', ['a']), graphTask('a'), graphTask('c'), graphTask('d', ['b', 'c'])],
      memory_stack_ids: ['notes-1'],
    });
    expect(spec.stopOnFailure).toBe(true);
    expect(spec.memoryStackIds).toEqual(['notes-1']);
    expect(spec.phases).toHaveLength(3);
    expect(spec.phases[0].agents.map(a => a.description)).toEqual(['Task a', 'Task c']);
    expect(spec.phases[1].agents.map(a => a.description)).toEqual(['Task b']);
    expect(spec.phases[2].agents.map(a => a.description)).toEqual(['Task d']);
    expect(spec.phases[2].agents[0].prompt).toContain('Prerequisites: b, c');
  });

  it('splits wide independent tasks but enforces phase/agent ceilings', () => {
    const spec = compileSwarmTaskGraph({ name: 'fan-out', tasks: Array.from({ length: 16 }, (_, i) => graphTask('a' + i)) });
    expect(spec.phases).toHaveLength(3);
    expect(spec.phases.every(p => p.agents.length <= 6)).toBe(true);
    expect(spec.phases.length).toBeLessThanOrEqual(MAX_WORKFLOW_PHASES);
    expect(spec.phases.flatMap(p => p.agents).length).toBeLessThanOrEqual(MAX_WORKFLOW_AGENTS);
  });

  it('schedules a specifically chosen paired peer without widening code permissions', () => {
    const spec = compileSwarmTaskGraph({
      name: 'distributed-read-only',
      tasks: [
        { ...graphTask('inspect'), peer: 'paired-node' },
        graphTask('review', ['inspect']),
      ],
    });
    expect(spec.phases[0].agents[0].peer).toBe('paired-node');
    expect(spec.phases[0].agents[0].profile).toBe('explore');
    expect(() => compileSwarmTaskGraph({
      name: 'remote-mutating',
      tasks: [{ ...graphTask('fix'), peer: 'paired-node', profile: 'code' }],
    })).toThrow(/read-only/);
  });

  it('rejects cycles, self-dependencies, unknown ids and duplicates', () => {
    expect(() => compileSwarmTaskGraph({ name: 'bad', tasks: [graphTask('a', ['b']), graphTask('b', ['a'])] })).toThrow(/cycle/);
    expect(() => compileSwarmTaskGraph({ name: 'bad', tasks: [graphTask('a', ['a'])] })).toThrow(/itself/);
    expect(() => compileSwarmTaskGraph({ name: 'bad', tasks: [graphTask('a', ['missing'])] })).toThrow(/Unknown dependency/);
    expect(() => compileSwarmTaskGraph({ name: 'bad', tasks: [graphTask('a'), graphTask('a')] })).toThrow(/unique/);
  });

  it('rejects unknown workflow fields and excessive serial depth', () => {
    expect(() => compileSwarmTaskGraph({ name: 'bad', tasks: [graphTask('a')], phases: [] })).toThrow(/cannot be combined/);
    const serial = Array.from({ length: 7 }, (_, i) => graphTask('a' + i, i ? ['a' + (i - 1)] : []));
    expect(() => compileSwarmTaskGraph({ name: 'deep', tasks: serial })).toThrow(/six phases/);
  });
});
