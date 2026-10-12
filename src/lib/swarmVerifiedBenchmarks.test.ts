import { beforeEach, describe, expect, it } from 'vitest';
import {
  clearSwarmBenchmark, importSwarmBenchmark, loadSwarmBenchmark,
  parseLiveSwarmBenchmark, rankEligibleSwarmTargets, setSwarmBenchmarkRouting,
  swarmBenchmarkRoutingEnabled, SWARM_BENCHMARK_KEY, SWARM_BENCHMARK_OPT_IN,
} from './swarmVerifiedBenchmarks';

const now = Date.parse('2026-10-12T10:00:00.000Z');
const fixtures = ['logic-regression','authorization-regression','parsing-regression'];
function report() {
  const targets = ['ollama:bad-model','ollama:good-model'];
  const results = targets.flatMap((target, i) =>
    fixtures.map((fixture, j) => ({
      target, fixture, repeat:1, runId: 'run-model-test-' + i + j,
      verified: i === 1 || j === 0,
      durationMs: i === 1 ? 400+j : 800+j,
    })));
  return {
    version:1, benchmark:'live-task-verified-fixtures', mode:'real-model',
    generatedAt:'2026-10-12T09:30:00.000Z',
    results,
    ranking: [
      {target:'ollama:good-model',passes:3,attempts:3,accuracy:1,averageVerifiedMs:401},
      {target:'ollama:bad-model',passes:1,attempts:3,accuracy:0.3333,averageVerifiedMs:800},
    ],
  };
}
beforeEach(() => {
  localStorage.removeItem(SWARM_BENCHMARK_KEY);
  localStorage.removeItem(SWARM_BENCHMARK_OPT_IN);
});

describe('opt-in real-model accuracy routing', () => {
  it('imports complete verified fixture evidence and computes the ranking independently', () => {
    const imported=importSwarmBenchmark(report(),now);
    expect(imported.ranking[0].target).toBe('ollama:good-model');
    expect(loadSwarmBenchmark(now)?.ranking[0].accuracy).toBe(1);
    expect(swarmBenchmarkRoutingEnabled()).toBe(false);
    setSwarmBenchmarkRouting(true);
    // Valid only at current wall clock; the persisted sample has a 90-day TTL.
    expect(rankEligibleSwarmTargets(['ollama:bad-model','ollama:good-model'],[],imported,true))
      .toEqual(['ollama:good-model','ollama:bad-model']);
  });
  it('never overrides an explicit routing policy preference, opt-out, or eligibility', () => {
    const imported=parseLiveSwarmBenchmark(report(),now);
    const eligible=['ollama:bad-model','ollama:good-model'];
    expect(rankEligibleSwarmTargets(eligible,['ollama:bad-model'],imported,true)).toEqual(eligible);
    expect(rankEligibleSwarmTargets(eligible,[],imported,false)).toEqual(eligible);
    expect(rankEligibleSwarmTargets(['ollama:bad-model','provider:unbenchmarked'],[],imported,true))
      .toEqual(['ollama:bad-model','provider:unbenchmarked']);
    expect(rankEligibleSwarmTargets(['provider:unconfigured','ollama:good-model'],[],imported,true))
      .toEqual(['provider:unconfigured','ollama:good-model']);
  });
  it('rejects forged self-reported scores and incomplete/duplicated fixture evidence', () => {
    const faulty=report();
    faulty.ranking[0].passes=99;
    expect(() => parseLiveSwarmBenchmark(faulty,now)).toThrow(/does not match/);
    const partial=report();
    partial.results.pop();
    expect(() => parseLiveSwarmBenchmark(partial,now)).toThrow(/fixture/);
    const duplicate=report();
    duplicate.results[0]=duplicate.results[1];
    expect(() => parseLiveSwarmBenchmark(duplicate,now)).toThrow(/Duplicate/);
  });
  it('rejects claimed model success without a real durable run identifier', () => {
    const forged=report();
    forged.results[3].runId='synthetic';
    expect(() => parseLiveSwarmBenchmark(forged,now)).toThrow(/actual-run evidence/);
    const synthetic=report();
    synthetic.mode='synthetic';
    expect(() => parseLiveSwarmBenchmark(synthetic,now)).toThrow(/real-model/);
  });
  it('rejects stale benchmarks and clears the opt-in with its evidence', () => {
    const parsed=importSwarmBenchmark(report(),now);
    expect(parsed.ranking).toHaveLength(2);
    expect(loadSwarmBenchmark(now+91*24*60*60_000)).toBeNull();
    setSwarmBenchmarkRouting(true);
    clearSwarmBenchmark();
    expect(loadSwarmBenchmark(now)).toBeNull();
    expect(swarmBenchmarkRoutingEnabled()).toBe(false);
  });
});
