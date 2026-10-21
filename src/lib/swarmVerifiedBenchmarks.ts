/**
 * Explicitly imported, reproducible live-model benchmark quality observations.
 *
 * The imported JSON is operator-provided, NOT intrinsically trusted proof of
 * identity/accuracy. Reject partial/contradictory fixture sets and derive
 * rankings from underlying per-run evidence, never from self-reported scores.
 * Routing stays opt-in, subagent-only and strictly inside candidates which
 * the existing dispatch policy already found eligible.
 */
export const SWARM_BENCHMARK_KEY = 'little-monkey-swarm-verified-benchmark-v1';
export const SWARM_BENCHMARK_OPT_IN = 'little-monkey-swarm-benchmark-routing-v1';
export const SWARM_CONTINUOUS_QUALITY_OPT_IN='little-monkey-swarm-continuous-quality-v1';

const FIXTURES = ['logic-regression', 'authorization-regression', 'parsing-regression'] as const;
const MAX_AGE_MS = 90 * 24 * 60 * 60_000;

export interface SwarmBenchScore {
  target: string;
  passes: number;
  attempts: number;
  accuracy: number;
  averageVerifiedMs: number | null;
}
export interface SwarmBenchmarkSummary {
  generatedAt: string;
  importedAt: number;
  ranking: SwarmBenchScore[];
}

function finiteInt(value: unknown, min: number, max: number): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= min && value <= max;
}
function targetId(v: unknown): v is string {
  return typeof v === 'string' && v.length <= 150 && v.length > 2 &&
    /^(ollama|provider|local):[A-Za-z0-9_.:%/@+-]+$/.test(v) && !/[\x00-\x1f]/.test(v);
}
function score(v: SwarmBenchScore): string {
  return JSON.stringify([v.target, v.passes, v.attempts, v.accuracy, v.averageVerifiedMs]);
}
export function parseLiveSwarmBenchmark(json: unknown, now = Date.now()): SwarmBenchmarkSummary {
  if (!json || typeof json !== 'object' || Array.isArray(json)) throw new Error('Benchmark must be a JSON object.');
  const data = json as Record<string, unknown>;
  if (data.version !== 1 || data.mode !== 'real-model' || data.benchmark !== 'live-task-verified-fixtures') {
    throw new Error('Only Little Monkey real-model verified-fixture benchmark format v1 is accepted.');
  }
  if (typeof data.generatedAt !== 'string' || Number.isNaN(Date.parse(data.generatedAt))) {
    throw new Error('Benchmark timestamp is invalid.');
  }
  const generated = Date.parse(data.generatedAt);
  if (generated > now + 5 * 60_000 || now - generated > MAX_AGE_MS) {
    throw new Error('Benchmark is from the future or is older than 90 days.');
  }
  if (!Array.isArray(data.ranking) || !Array.isArray(data.results) ||
    data.ranking.length < 1 || data.ranking.length > 8 ||
    data.results.length < data.ranking.length * FIXTURES.length || data.results.length > 120) {
    throw new Error('Benchmark has an incomplete or oversized target/fixture set.');
  }
  const targets = new Set<string>();
  for (const raw of data.ranking) {
    if (!raw || typeof raw !== 'object' || !targetId((raw as {target?:unknown}).target)) {
      throw new Error('Benchmark contains invalid model target identifiers.');
    }
    const target = (raw as {target:string}).target;
    if (targets.has(target)) throw new Error('Benchmark contains duplicate targets.');
    targets.add(target);
  }
  const groups = new Map<string, Map<string, { verified: boolean; durationMs: number }>>();
  for (const raw of data.results) {
    if (!raw || typeof raw !== 'object') throw new Error('Malformed fixture result.');
    const result = raw as Record<string, unknown>;
    if (!targetId(result.target) || !targets.has(result.target) ||
      !FIXTURES.includes(result.fixture as (typeof FIXTURES)[number]) ||
      !finiteInt(result.repeat, 1, 5) || typeof result.verified !== 'boolean' ||
      !finiteInt(result.durationMs, 0, 3_600_000) ||
      (result.verified && (typeof result.runId !== 'string' || !/^run-[A-Za-z0-9-]{6,200}$/.test(result.runId)))) {
      throw new Error('Fixture result does not contain valid actual-run evidence.');
    }
    const key = result.fixture + ':' + result.repeat;
    const group = groups.get(result.target) ?? new Map();
    if (group.has(key)) throw new Error('Duplicate benchmark fixture observation.');
    group.set(key, { verified: result.verified, durationMs: result.durationMs });
    groups.set(result.target, group);
  }
  const ranking: SwarmBenchScore[] = [];
  for (const target of targets) {
    const rows = groups.get(target);
    if (!rows) throw new Error('Target has no completed benchmarks.');
    const repeats = rows.size / FIXTURES.length;
    if (!Number.isInteger(repeats) || repeats < 1 || repeats > 5) {
      throw new Error('Every model must have all three fixtures per repetition.');
    }
    for (let n = 1; n <= repeats; n++) {
      for (const fixture of FIXTURES) {
        if (!rows.has(fixture + ':' + n)) throw new Error('Benchmark is missing an independent fixture.');
      }
    }
    const successful = [...rows.values()].filter(row => row.verified);
    const attempts = rows.size;
    ranking.push({
      target,
      passes: successful.length, attempts,
      accuracy: Number((successful.length / attempts).toFixed(4)),
      averageVerifiedMs: successful.length
        ? Math.round(successful.reduce((sum, row) => sum + row.durationMs, 0) / successful.length) : null,
    });
  }
  ranking.sort((a,b) => b.accuracy-a.accuracy ||
    (a.averageVerifiedMs ?? Infinity)-(b.averageVerifiedMs ?? Infinity));
  if (data.ranking.length !== ranking.length || data.ranking.some((raw, i) =>
    score(raw as SwarmBenchScore) !== score(ranking[i]))) {
    throw new Error('Reported benchmark ranking does not match actual fixture outcomes.');
  }
  return { generatedAt: data.generatedAt, importedAt: now, ranking };
}

/** Persist the original independently graded per-run fixture observations,
 * not a writable summary. Re-run the parser on every routing read so an
 * edited/stale/incomplete local record cannot silently certify a new model.
 * Old v1 summary-only cache intentionally fails closed until re-imported. */
interface StoredSwarmBenchmarkEvidence {
  storageVersion: 2;
  importedAt: number;
  evidence: unknown;
  automatic?: boolean;
}
export function loadSwarmBenchmark(now = Date.now()): SwarmBenchmarkSummary | null {
  try {
    const raw = localStorage.getItem(SWARM_BENCHMARK_KEY);
    if (!raw || raw.length > 128_000 || !Number.isSafeInteger(now) || now<=0) return null;
    const saved: unknown = JSON.parse(raw);
    if (!saved || typeof saved !== 'object' || Array.isArray(saved)) return null;
    const stored = saved as Partial<StoredSwarmBenchmarkEvidence>;
    if (stored.storageVersion!==2 || !finiteInt(stored.importedAt, 1, now+5*60_000) ||
        stored.evidence===undefined || now-stored.importedAt>MAX_AGE_MS) return null;
    // Continuous mode is fail-closed. A manual import or an old passing
    // fixture cannot keep an autonomous optimizer enabled after the live
    // independent evaluator stops. Only fresh automatic source evidence counts.
    const automatic=localStorage.getItem(SWARM_CONTINUOUS_QUALITY_OPT_IN)==='true';
    if(automatic&&(stored.automatic!==true||
      now-stored.importedAt>36*60*60_000))return null;
    const verified=parseLiveSwarmBenchmark(stored.evidence,now);
    if(automatic&&now-Date.parse(verified.generatedAt)>36*60*60_000)return null;
    // Parse may be given a malicious generatedAt; it rejects stale/future
    // and rebuilds all scores from fixture observations rather than trusting
    // a reported ranking.
    return {...verified,importedAt:stored.importedAt};
  } catch { return null; }
}

export function importSwarmBenchmark(value: unknown, now = Date.now(),automatic=false): SwarmBenchmarkSummary {
  const parsed = parseLiveSwarmBenchmark(value, now);
  const source=JSON.stringify(value);
  if(source.length>128_000)throw new Error('Benchmark evidence exceeds retention limits.');
  localStorage.setItem(SWARM_BENCHMARK_KEY, JSON.stringify({
    storageVersion:2,importedAt:now,evidence:value,automatic,
  } satisfies StoredSwarmBenchmarkEvidence));
  return parsed;
}
/** Continuous mode is explicitly enrolled and cannot silently fall back to
 * a human-imported 90-day-old score after the background evaluator stops. */
export function setSwarmContinuousQuality(enabled:boolean):void{
  localStorage.setItem(SWARM_CONTINUOUS_QUALITY_OPT_IN,enabled?'true':'false');
}
export function swarmContinuousQualityEnabled():boolean{
  try{return localStorage.getItem(SWARM_CONTINUOUS_QUALITY_OPT_IN)==='true';}
  catch{return false;}
}
export function invalidateSwarmContinuousQuality():void{
  if(swarmContinuousQualityEnabled()){
    localStorage.removeItem(SWARM_BENCHMARK_KEY);
  }
}
export function setSwarmBenchmarkRouting(enabled: boolean): void {
  localStorage.setItem(SWARM_BENCHMARK_OPT_IN, enabled ? 'true' : 'false');
}
export function swarmBenchmarkRoutingEnabled(): boolean {
  try { return localStorage.getItem(SWARM_BENCHMARK_OPT_IN) === 'true' && !!loadSwarmBenchmark(); }
  catch { return false; }
}
export function clearSwarmBenchmark(): void {
  localStorage.removeItem(SWARM_BENCHMARK_KEY);
  setSwarmBenchmarkRouting(false);
}

/** Never expands eligibility or bypasses manual policy preference ordering.
 * Caller supplies ONLY keys already admitted by the existing model router. */
export function rankEligibleSwarmTargets(
  eligibleKeys: readonly string[],
  preferredKeys: readonly string[],
  summary: SwarmBenchmarkSummary | null = loadSwarmBenchmark(),
  enabled = swarmBenchmarkRoutingEnabled(),
): string[] {
  if (!enabled || !summary || eligibleKeys.length < 2 || preferredKeys.length > 0) return [...eligibleKeys];
  const scores = new Map(summary.ranking.filter(r => r.attempts >= 3).map(r => [r.target, r]));
  const observed = eligibleKeys.filter(key => scores.has(key));
  if (observed.length < 2) return [...eligibleKeys];
  // Quality improvement is demonstrable ONLY among measured models.
  // Never promote an unknown or ineligible target into the existing sequence.
  const ranked = [...observed].sort((a,b) => {
    const left=scores.get(a)!,right=scores.get(b)!;
    const accuracy = right.accuracy-left.accuracy;
    if (accuracy) return accuracy;
    const latencyA = left.averageVerifiedMs ?? Infinity;
    const latencyB = right.averageVerifiedMs ?? Infinity;
    if (latencyA !== latencyB) return latencyA < latencyB ? -1 : 1;
    return eligibleKeys.indexOf(a)-eligibleKeys.indexOf(b);
  });
  return [...ranked, ...eligibleKeys.filter(key => !scores.has(key))];
}
