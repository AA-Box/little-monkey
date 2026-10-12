import { describe, expect, it } from 'vitest';
import { buildGoalSwarm, parseSwarmMemoryStackIds, selectSwarmDomain } from './swarmPlanning';
import { MAX_AGENTS_PER_PHASE, MAX_WORKFLOW_AGENTS, MAX_WORKFLOW_PHASES } from './workflow';

describe('goal-driven native agent swarms', () => {
  it.each([
    ['audit authorization and security', 'security'],
    ['reduce playback latency', 'performance'],
    ['improve responsive UI', 'frontend'],
    ['refactor database migration', 'backend'],
    ['design a new component', 'general'],
  ] as const)('routes %s to %s specialists', (goal, domain) => {
    expect(selectSwarmDomain(goal)).toBe(domain);
  });

  it('compiles a read-only analysis swarm with strong caps', () => {
    const spec = buildGoalSwarm({ goal: 'Improve frontend accessibility', scope: 'src/components' });
    const agents = spec.phases.flatMap(phase => phase.agents);
    expect(spec.name).toMatch(/^auto-swarm-analysis-improve-frontend-[a-z0-9-]+-[0-9a-f]{8}$/);
    expect(spec.phases.length).toBeLessThanOrEqual(MAX_WORKFLOW_PHASES);
    expect(spec.phases.every(p => p.agents.length <= MAX_AGENTS_PER_PHASE)).toBe(true);
    expect(agents.length).toBeLessThanOrEqual(MAX_WORKFLOW_AGENTS);
    expect(agents.every(a => a.profile === 'explore')).toBe(true);
    expect(agents.every(a => a.prompt.includes('src/components'))).toBe(true);
    expect(agents[0].description).toContain('Frontend');
  });

  it('isolates mutations to one implementation phase and adds verification', () => {
    const spec = buildGoalSwarm({ goal: 'Implement API caching', mode: 'implement' });
    const agents = spec.phases.flatMap(p => p.agents);
    expect(spec.name).toMatch(/^auto-swarm-implementation-implement-api-caching-[0-9a-f]{8}$/);
    expect(spec.phases).toHaveLength(5);
    expect(spec.phases[2].agents).toHaveLength(1);
    expect(spec.phases[2].agents[0].profile).toBe('code');
    expect(spec.phases[2].agents[0].prompt).toMatch(/approval prompts/);
    expect(spec.phases[3].agents.some(a => a.description === 'Test execution reviewer' && a.profile === 'code')).toBe(true);
    expect(agents.length).toBeLessThanOrEqual(MAX_WORKFLOW_AGENTS);
  });

  it('avoids collisions between saved workflows for different goals', () => {
    const left = buildGoalSwarm({ goal: 'Analyze API authentication' });
    const same = buildGoalSwarm({ goal: 'Analyze API authentication' });
    const right = buildGoalSwarm({ goal: 'Analyze API performance' });
    expect(left.name).toBe(same.name);
    expect(left.name).not.toBe(right.name);
    expect(buildGoalSwarm({ goal: 'Analyze API authentication', scope: 'repo-a' }).name)
      .not.toBe(buildGoalSwarm({ goal: 'Analyze API authentication', scope: 'repo-b' }).name);
  });

  it('distributes only explicitly selected read-only specialists to paired peers', () => {
    const spec = buildGoalSwarm({
      goal: 'Analyze API security', mode: 'analyze', peer_aliases: ['node-a', 'node-b'],
    });
    expect(spec.phases[0].agents.map(a => a.peer ?? null)).toEqual(['node-a', 'node-b', null, null]);
    expect(spec.phases.slice(1).flatMap(p => p.agents).every(a => !a.peer)).toBe(true);
    expect(() => buildGoalSwarm({
      goal: 'Implement API security', mode: 'implement', peer_aliases: ['node-a'],
    })).toThrow(/read-only/);
    expect(() => buildGoalSwarm({
      goal: 'Analyze API security', peer_aliases: ['node-a'], memory_stack_ids: ['private-index'],
    })).toThrow(/cannot be automatically forwarded/);
    expect(() => buildGoalSwarm({
      goal: 'Analyze API security', peer_aliases: ['node-a', 'node-a'],
    })).toThrow(/duplicates/);
  });

  it('offers only explicitly approved peer candidates for adaptive read-only discovery', () => {
    const spec = buildGoalSwarm({
      goal: 'Review backend reliability', mode: 'analyze', auto_peer_pool: ['node-a', 'node-b'],
    });
    const discoverers = spec.phases[0].agents;
    expect(discoverers).toHaveLength(4);
    expect(discoverers.every(agent => agent.profile === 'explore')).toBe(true);
    expect(discoverers[0].peer).toBe('node-a');
    expect(discoverers[0].peerCandidates).toEqual(['node-a', 'node-b']);
    expect(discoverers[1].peerCandidates).toEqual(['node-b', 'node-a']);
    expect(() => buildGoalSwarm({ goal: 'Fix backend', mode: 'implement', auto_peer_pool: ['node-a'] })).toThrow(/read-only/);
    expect(() => buildGoalSwarm({ goal: 'Review backend', auto_peer_pool: ['node-a'], memory_stack_ids: ['private'] })).toThrow(/cannot be automatically forwarded/);
    expect(() => buildGoalSwarm({ goal: 'Review backend', auto_peer_pool: ['node-a'], peer_aliases: ['node-b'] })).toThrow(/not both/);
  });

  it('adds strict independent reviewers and opt-in notebook with no remote forwarding', () => {
    const spec = buildGoalSwarm({ goal: 'Audit API auth', consensus: 'strict', notebook: true });
    expect(spec.consensus).toBe('strict');
    expect(spec.notebook).toBe(true);
    expect(spec.phases[1].agents).toHaveLength(2);
    expect(spec.phases[1].agents.filter(a => a.prompt.toLowerCase().includes('claim'))).toHaveLength(2);
    expect(() => buildGoalSwarm({ goal: 'Audit auth', mode: 'implement', consensus: 'strict' })).toThrow(/read-only/);
    expect(() => buildGoalSwarm({ goal: 'Audit auth', notebook: true, peer_aliases: ['node-a'] })).toThrow(/cannot be automatically forwarded/);
  });

  it('provides opt-in named persistent knowledge stack references', () => {
    const spec = buildGoalSwarm({ goal: 'Investigate schema correctness', memory_stack_ids: ['stack-1', 'stack_2'] });
    expect(spec.memoryStackIds).toEqual(['stack-1', 'stack_2']);
    expect(buildGoalSwarm({ goal: 'Investigate schema correctness' }).memoryStackIds).toBeUndefined();
  });

  it('rejects unknown modes, interleaved workflow definitions and malformed input', () => {
    expect(() => buildGoalSwarm({ goal: 'work', mode: 'delete-everything' })).toThrow(/mode/);
    expect(() => buildGoalSwarm({ goal: 'work', phases: [] })).toThrow(/cannot be combined/);
    expect(() => buildGoalSwarm({ goal: '' })).toThrow(/goal/);
    expect(() => buildGoalSwarm({ goal: 'x'.repeat(1201) })).toThrow(/goal/);
    expect(() => parseSwarmMemoryStackIds(['good', 'bad/path'])).toThrow(/memory_stack_ids/);
    expect(() => parseSwarmMemoryStackIds(['repeat', 'repeat'])).toThrow(/duplicate/);
    expect(() => parseSwarmMemoryStackIds(Array(6).fill('x'))).toThrow(/memory_stack_ids/);
  });
});
