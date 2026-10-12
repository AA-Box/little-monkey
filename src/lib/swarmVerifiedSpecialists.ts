/**
 * Evidence-gated specialist QUALITY selection.
 *
 * Reliability counters measure successful completion, not correctness.
 * This independent opt-in source accepts actual model run IDs with complete
 * externally checked code fixtures and recomputes every score. No learned
 * prompt, credential or provider permission is stored here.
 *
 * NOTE: Imported JSON is operator supplied, not cryptographic attestation.
 */
export const VERIFIED_SPECIALISTS_KEY = 'little-monkey-swarm-verified-specialists-v1';
export const VERIFIED_SPECIALISTS_OPT_IN = 'little-monkey-swarm-verified-specialists-enabled-v1';
const MAX_AGE = 90 * 24 * 60 * 60_000;
const FIXTURES = ['auth-bypass', 'code-injection', 'safe-control'] as const;
export const SECURITY_SPECIALISTS = [
  'Threat model reviewer', 'Secrets exposure reviewer', 'Race condition reviewer',
] as const;
export type VerifiedSpecialistDomain = 'security';
export interface VerifiedSpecialistRow {
  specialist: string;
  passes: number;
  attempts: number;
  accuracy: number;
  averageVerifiedMs: number | null;
}
export interface VerifiedSpecialistSummary {
  domain: VerifiedSpecialistDomain;
  target: string;
  generatedAt: string;
  importedAt: number;
  ranking: VerifiedSpecialistRow[];
}
function boundedInt(v: unknown, min: number, max: number): v is number {
  return typeof v === 'number' && Number.isSafeInteger(v) && v >= min && v <= max;
}
function validTarget(v: unknown): v is string {
  return typeof v === 'string' && v.length <= 150 &&
    /^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(v);
}
function equalRow(a: unknown, b: VerifiedSpecialistRow): boolean {
  if (!a || typeof a !== 'object' || Array.isArray(a)) return false;
  const record = a as Record<string, unknown>;
  return Object.keys(record).sort().join(',') ===
    'accuracy,attempts,averageVerifiedMs,passes,specialist' &&
    record.specialist === b.specialist && record.passes === b.passes &&
    record.attempts === b.attempts && record.accuracy === b.accuracy &&
    record.averageVerifiedMs === b.averageVerifiedMs;
}

/** Import is transactional. Incomplete or forged aggregate claims are rejected. */
export function parseVerifiedSpecialistBenchmark(input: unknown, now = Date.now()): VerifiedSpecialistSummary {
  if (!input || typeof input !== 'object' || Array.isArray(input)) {
    throw new Error('Verified specialist benchmark must be a JSON object.');
  }
  const data = input as Record<string, unknown>;
  if (data.version !== 1 || data.benchmark !== 'live-specialist-source-fixtures' ||
      data.mode !== 'real-model' || data.domain !== 'security' || !validTarget(data.target)) {
    throw new Error('Only installed-model security specialist fixture format v1 is accepted.');
  }
  const time = typeof data.generatedAt === 'string' ? Date.parse(data.generatedAt) : NaN;
  if (!Number.isFinite(time) || time > now + 5 * 60_000 || time < now - MAX_AGE) {
    throw new Error('Benchmark is stale or has an invalid timestamp.');
  }
  if (!Array.isArray(data.results) || !Array.isArray(data.ranking) ||
      data.ranking.length !== SECURITY_SPECIALISTS.length ||
      data.results.length < 9 || data.results.length > 45) {
    throw new Error('Every specialist must have all source fixtures.');
  }
  const observed = new Map<string, Map<string, {pass:boolean; ms:number}>>();
  const runIds = new Set<string>();
  for (const row of data.results) {
    if (!row || typeof row !== 'object' || Array.isArray(row)) throw new Error('Malformed specialist result.');
    const r = row as Record<string, unknown>;
    if (typeof r.specialist !== 'string' ||
        !SECURITY_SPECIALISTS.includes(r.specialist as (typeof SECURITY_SPECIALISTS)[number]) ||
        !FIXTURES.includes(r.fixture as (typeof FIXTURES)[number]) ||
        !boundedInt(r.repeat, 1, 5) || !boundedInt(r.durationMs, 0, 3_600_000) ||
        typeof r.verified !== 'boolean' ||
        typeof r.runId !== 'string' || !/^run-[A-Za-z0-9-]{6,200}$/.test(r.runId) ||
        runIds.has(r.runId)) {
      throw new Error('Every fixture must reference a unique actual model run and bounded independent verdict.');
    }
    runIds.add(r.runId);
    const key = r.fixture + ':' + r.repeat;
    const runs = observed.get(r.specialist) ?? new Map();
    if (runs.has(key)) throw new Error('Duplicate specialist fixture observation.');
    runs.set(key, {pass:r.verified, ms:r.durationMs});
    observed.set(r.specialist, runs);
  }
  const rows: VerifiedSpecialistRow[] = [];
  for (const specialist of SECURITY_SPECIALISTS) {
    const results = observed.get(specialist);
    if (!results || results.size % FIXTURES.length !== 0) throw new Error('Incomplete specialist fixture coverage.');
    const repeats = results.size / FIXTURES.length;
    if (!boundedInt(repeats, 1, 5)) throw new Error('Invalid repetition count.');
    for (let n = 1; n <= repeats; n++) for (const fixture of FIXTURES) {
      if (!results.has(fixture + ':' + n)) throw new Error('Missing independent source fixture.');
    }
    const positives = [...results.values()].filter(row => row.pass);
    rows.push({
      specialist,
      passes: positives.length,
      attempts: results.size,
      accuracy: Number((positives.length / results.size).toFixed(4)),
      averageVerifiedMs: positives.length
        ? Math.round(positives.reduce((sum,row) => sum + row.ms, 0) / positives.length) : null,
    });
  }
  rows.sort((a,b) => b.accuracy-a.accuracy ||
    (a.averageVerifiedMs ?? Infinity) - (b.averageVerifiedMs ?? Infinity) ||
    SECURITY_SPECIALISTS.indexOf(a.specialist as (typeof SECURITY_SPECIALISTS)[number]) -
      SECURITY_SPECIALISTS.indexOf(b.specialist as (typeof SECURITY_SPECIALISTS)[number]));
  if (!(data.ranking as unknown[]).every((row,i) => equalRow(row,rows[i]))) {
    throw new Error('Specialist ranking contradicts real fixture observations.');
  }
  return {domain:'security',target:data.target,generatedAt:data.generatedAt as string,importedAt:now,ranking:rows};
}
export function importVerifiedSpecialists(input: unknown, now = Date.now()): VerifiedSpecialistSummary {
  const parsed = parseVerifiedSpecialistBenchmark(input,now);
  localStorage.setItem(VERIFIED_SPECIALISTS_KEY,JSON.stringify(parsed));
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,'false');
  return parsed;
}
export function loadVerifiedSpecialists(now = Date.now()): VerifiedSpecialistSummary | null {
  try {
    const raw = localStorage.getItem(VERIFIED_SPECIALISTS_KEY);
    if (!raw) return null;
    const data: unknown = JSON.parse(raw);
    if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
    const summary = data as Partial<VerifiedSpecialistSummary>;
    const time = typeof summary.generatedAt === 'string' ? Date.parse(summary.generatedAt) : NaN;
    if (summary.domain !== 'security' || !validTarget(summary.target) || !Number.isFinite(time) ||
        time < now-MAX_AGE || time > now+5*60_000 ||
        !boundedInt(summary.importedAt,0,now+5*60_000) ||
        !Array.isArray(summary.ranking) || summary.ranking.length !== 3) return null;
    if (new Set(summary.ranking.map(x => x.specialist)).size !== 3 ||
        !summary.ranking.every(x => SECURITY_SPECIALISTS.includes(x.specialist as (typeof SECURITY_SPECIALISTS)[number]) &&
          boundedInt(x.attempts,3,15) && boundedInt(x.passes,0,x.attempts) &&
          x.attempts % 3 === 0 && x.accuracy === Number((x.passes / x.attempts).toFixed(4)) &&
          (x.averageVerifiedMs === null || boundedInt(x.averageVerifiedMs,0,3_600_000)))) return null;
    return summary as VerifiedSpecialistSummary;
  } catch { return null; }
}
export function verifiedSpecialistRoutingEnabled(): boolean {
  try { return localStorage.getItem(VERIFIED_SPECIALISTS_OPT_IN)==='true' && !!loadVerifiedSpecialists(); }
  catch {return false;}
}
export function setVerifiedSpecialistRouting(value: boolean): void {
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,value && !!loadVerifiedSpecialists() ? 'true' : 'false');
}
export function clearVerifiedSpecialists(): void {
  localStorage.removeItem(VERIFIED_SPECIALISTS_KEY);
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,'false');
}
/** Do not expand role eligibility or change other engineering domains. */
export function chooseVerifiedSpecialist(
  domain: string, options: readonly string[],
  summary: VerifiedSpecialistSummary | null = loadVerifiedSpecialists(),
  enabled = verifiedSpecialistRoutingEnabled(),
): string | null {
  if (!enabled || domain !== 'security' || !summary || summary.domain !== domain) return null;
  const measured = new Map(summary.ranking.filter(r => r.attempts >= 3).map(r => [r.specialist,r]));
  const admissible = options.filter(option => measured.has(option));
  if (admissible.length < 2) return null;
  admissible.sort((a,b) => {
    const aa = measured.get(a)!, bb = measured.get(b)!;
    return bb.accuracy-aa.accuracy ||
      (aa.averageVerifiedMs ?? Infinity)-(bb.averageVerifiedMs ?? Infinity) ||
      options.indexOf(a)-options.indexOf(b);
  });
  return admissible[0];
}
