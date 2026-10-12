/**
 * Independently graded, operator-imported specialist quality data.
 * Reliability (completed without error) is explicitly NOT model accuracy.
 * A domain is routed by quality only if the operator opted in to a complete,
 * non-expired, externally graded run set for exactly that domain.
 *
 * An imported JSON file is not cryptographic provenance; real CLI execution
 * is validated by the executable source oracle in the benchmark runner.
 */
export const VERIFIED_SPECIALISTS_KEY = 'little-monkey-swarm-verified-specialists-v1';
export const VERIFIED_SPECIALISTS_OPT_IN = 'little-monkey-swarm-verified-specialists-enabled-v1';
export const SPECIALIST_DOMAINS = ['security','frontend','backend','performance','general'] as const;
export type VerifiedSpecialistDomain = typeof SPECIALIST_DOMAINS[number];
export const SPECIALIST_ROLES: Record<VerifiedSpecialistDomain, readonly string[]> = {
  security: ['Threat model reviewer','Secrets exposure reviewer','Race condition reviewer'],
  frontend: ['Frontend performance reviewer','Browser security reviewer','UI testability reviewer'],
  backend: ['Backend caching reviewer','API resilience reviewer','Data privacy reviewer'],
  performance: ['Queueing reviewer','Memory and cache reviewer','Capacity reviewer'],
  general: ['Documentation reviewer','Incident response reviewer','Integration reviewer'],
};
export const SECURITY_SPECIALISTS = SPECIALIST_ROLES.security;
export const SPECIALIST_FIXTURES: Record<VerifiedSpecialistDomain, readonly string[]> = {
  security: ['auth-bypass','code-injection','safe-control'],
  frontend: ['event-handler','accessibility-label','safe-control'],
  backend: ['tenant-leak','timeout-budget','safe-control'],
  performance: ['quadratic-scan','cache-recompute','safe-control'],
  general: ['configuration-flag','integration-contract','safe-control'],
};
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
interface PersistedScores {
  version: 2;
  domains: Partial<Record<VerifiedSpecialistDomain, VerifiedSpecialistSummary>>;
}
interface PersistedOptIns {
  version: 2;
  domains: Partial<Record<VerifiedSpecialistDomain, boolean>>;
}
const MAX_AGE = 90 * 24 * 60 * 60_000;
const MAX_BENCHMARK_BYTES = 100_000;
function domainValid(value: unknown): value is VerifiedSpecialistDomain {
  return typeof value === 'string' && (SPECIALIST_DOMAINS as readonly string[]).includes(value);
}
function integer(value: unknown, low: number, high: number): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= low && value <= high;
}
function targetValid(value: unknown): value is string {
  return typeof value === 'string' && value.length <= 150 &&
    /^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(value);
}
function rankingEquals(raw: unknown, expected: VerifiedSpecialistRow): boolean {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return false;
  const r = raw as Record<string,unknown>;
  return Object.keys(r).sort().join(',') ===
    'accuracy,attempts,averageVerifiedMs,passes,specialist' &&
    r.specialist === expected.specialist && r.passes === expected.passes &&
    r.attempts === expected.attempts && r.accuracy === expected.accuracy &&
    r.averageVerifiedMs === expected.averageVerifiedMs;
}
function summaryValid(raw: unknown, domain: VerifiedSpecialistDomain, now: number): raw is VerifiedSpecialistSummary {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return false;
  const s = raw as Partial<VerifiedSpecialistSummary>;
  const ts = typeof s.generatedAt === 'string' ? Date.parse(s.generatedAt) : NaN;
  const roles = SPECIALIST_ROLES[domain];
  if (s.domain !== domain || !targetValid(s.target) || !Number.isFinite(ts) ||
      ts > now + 300_000 || now-ts > MAX_AGE ||
      !integer(s.importedAt,0,now+300_000) ||
      !Array.isArray(s.ranking) || s.ranking.length !== roles.length ||
      new Set(s.ranking.map(row => row.specialist)).size !== roles.length ||
      !s.ranking.every(row => roles.includes(row.specialist) &&
        integer(row.attempts,3,15) && row.attempts % 3 === 0 &&
        integer(row.passes,0,row.attempts) &&
        row.accuracy === Number((row.passes / row.attempts).toFixed(4)) &&
        (row.averageVerifiedMs === null || integer(row.averageVerifiedMs,0,3_600_000)))) return false;
  const sorted = [...s.ranking].sort((a,b) => b.accuracy-a.accuracy ||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity) ||
    roles.indexOf(a.specialist)-roles.indexOf(b.specialist));
  return sorted.every((row,i) => rankingEquals(s.ranking![i],row));
}
function storedSummaries(now: number): PersistedScores {
  const empty: PersistedScores = {version:2,domains:{}};
  try {
    const text = localStorage.getItem(VERIFIED_SPECIALISTS_KEY);
    if (!text || text.length > MAX_BENCHMARK_BYTES) return empty;
    const value: unknown = JSON.parse(text);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return empty;
    const data = value as Record<string,unknown>;
    // On-disk v1 contained a single security summary. Migrate lazily and
    // without accepting the unknown fields of an untrusted local JSON.
    const candidate: Record<string,unknown> = data.version === 2 && data.domains &&
      typeof data.domains === 'object' && !Array.isArray(data.domains)
        ? data.domains as Record<string,unknown>
        : { security:value };
    if (Object.keys(candidate).some(key=>!domainValid(key))) return empty;
    for (const domain of SPECIALIST_DOMAINS) {
      if (summaryValid(candidate[domain],domain,now)) empty.domains[domain]=candidate[domain];
    }
  } catch { /* broken/oversized local storage never authorizes routing */ }
  return empty;
}
function storedFlags(): PersistedOptIns {
  const empty: PersistedOptIns = {version:2,domains:{}};
  try {
    const raw = localStorage.getItem(VERIFIED_SPECIALISTS_OPT_IN);
    if (raw === 'true') return {version:2,domains:{security:true}}; // legacy security-only opt-in
    if (!raw || raw.length > 1000) return empty;
    const object: unknown=JSON.parse(raw);
    if (!object || typeof object !== 'object' || Array.isArray(object)) return empty;
    const value=object as Record<string,unknown>;
    if (value.version!==2 || !value.domains || typeof value.domains!=='object' ||
      Array.isArray(value.domains)) return empty;
    const domains=value.domains as Record<string,unknown>;
    if (Object.keys(domains).some(key=>!domainValid(key) || typeof domains[key]!=='boolean'))return empty;
    return {version:2,domains:domains as PersistedOptIns['domains']};
  } catch {return empty;}
}
export function parseVerifiedSpecialistBenchmark(value: unknown, now=Date.now()): VerifiedSpecialistSummary {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Verified specialist benchmark must be a JSON object.');
  }
  const data=value as Record<string,unknown>;
  if (!domainValid(data.domain) || !targetValid(data.target) ||
      data.benchmark!=='live-specialist-source-fixtures' || data.mode!=='real-model' ||
      !(data.version===2 || (data.version===1 && data.domain==='security'))) {
    throw new Error('Unsupported real-model specialist domain/fixture benchmark version.');
  }
  const domain=data.domain;
  const roles=SPECIALIST_ROLES[domain];
  const fixtures=SPECIALIST_FIXTURES[domain];
  const ts=typeof data.generatedAt==='string'?Date.parse(data.generatedAt):NaN;
  if (!Number.isFinite(ts) || ts>now+300_000 || now-ts>MAX_AGE) {
    throw new Error('Specialist benchmark is stale or has an invalid timestamp.');
  }
  if (!Array.isArray(data.results) || !Array.isArray(data.ranking) ||
      data.ranking.length !== roles.length ||
      data.results.length<roles.length*fixtures.length ||
      data.results.length>roles.length*fixtures.length*5) {
    throw new Error('Every specialist must have all independently graded source fixtures.');
  }
  const observed = new Map<string,Map<string,{passed:boolean; duration:number}>>();
  const realRuns = new Set<string>();
  for (const raw of data.results) {
    if (!raw || typeof raw!=='object' || Array.isArray(raw)) throw new Error('Malformed fixture result.');
    const row=raw as Record<string,unknown>;
    if (typeof row.specialist!=='string' || !roles.includes(row.specialist) ||
        typeof row.fixture!=='string' || !fixtures.includes(row.fixture) ||
        !integer(row.repeat,1,5) || !integer(row.durationMs,0,3_600_000) ||
        typeof row.verified!=='boolean' ||
        (row.runId!==null && (typeof row.runId!=='string' ||
          !/^run-[A-Za-z0-9-]{6,200}$/.test(row.runId))) ||
        (row.verified && typeof row.runId!=='string')) {
      throw new Error('Fixture lacks a valid run receipt or independent checked verdict.');
    }
    if (typeof row.runId==='string') {
      if (realRuns.has(row.runId)) throw new Error('Duplicate actual model run ID.');
      realRuns.add(row.runId);
    }
    const rows=observed.get(row.specialist)??new Map();
    const key=row.fixture+':'+row.repeat;
    if (rows.has(key))throw new Error('Duplicate specialist fixture observation.');
    rows.set(key,{passed:row.verified,duration:row.durationMs});
    observed.set(row.specialist,rows);
  }
  const ranking:VerifiedSpecialistRow[]=[];
  for (const specialist of roles) {
    const data=observed.get(specialist);
    if (!data || data.size%fixtures.length!==0) throw new Error('Incomplete specialist fixture coverage.');
    const repeats=data.size/fixtures.length;
    if (!integer(repeats,1,5)) throw new Error('Invalid specialist repetition count.');
    for (let repeat=1;repeat<=repeats;repeat++)for(const fixture of fixtures) {
      if (!data.has(fixture+':'+repeat))throw new Error('Missing independent source fixture.');
    }
    const passing=[...data.values()].filter(row=>row.passed);
    ranking.push({specialist,passes:passing.length,attempts:data.size,
      accuracy:Number((passing.length/data.size).toFixed(4)),
      averageVerifiedMs:passing.length
        ? Math.round(passing.reduce((sum,row)=>sum+row.duration,0)/passing.length) : null});
  }
  ranking.sort((a,b)=>b.accuracy-a.accuracy ||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity) ||
    roles.indexOf(a.specialist)-roles.indexOf(b.specialist));
  if (!(data.ranking as unknown[]).every((row,i)=>rankingEquals(row,ranking[i]))) {
    throw new Error('Reported benchmark ranking contradicts independently graded fixture outcomes.');
  }
  return {domain,target:data.target,generatedAt:data.generatedAt as string,importedAt:now,ranking};
}
export function loadVerifiedSpecialists(
  domain:VerifiedSpecialistDomain='security', now=Date.now(),
): VerifiedSpecialistSummary|null {
  return storedSummaries(now).domains[domain]??null;
}
export function listVerifiedSpecialists(now=Date.now()): VerifiedSpecialistSummary[] {
  const saved=storedSummaries(now).domains;
  const summaries: VerifiedSpecialistSummary[]=[];
  for (const domain of SPECIALIST_DOMAINS) {
    const summary=saved[domain];
    if (summary) summaries.push(summary);
  }
  return summaries;
}
export function importVerifiedSpecialists(value:unknown,now=Date.now()):VerifiedSpecialistSummary {
  const parsed=parseVerifiedSpecialistBenchmark(value,now);
  const saved=storedSummaries(now);
  const flags=storedFlags();
  saved.domains[parsed.domain]=parsed;
  flags.domains[parsed.domain]=false; // an import always requires renewed approval for that domain
  localStorage.setItem(VERIFIED_SPECIALISTS_KEY,JSON.stringify(saved));
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,JSON.stringify(flags));
  return parsed;
}
export function verifiedSpecialistRoutingEnabled(domain:VerifiedSpecialistDomain='security'):boolean {
  try{return storedFlags().domains[domain]===true && !!loadVerifiedSpecialists(domain);}
  catch{return false;}
}
export function setVerifiedSpecialistRouting(
  enabled:boolean,domain:VerifiedSpecialistDomain='security',
):void {
  const flags=storedFlags();
  flags.domains[domain]=enabled && !!loadVerifiedSpecialists(domain);
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,JSON.stringify(flags));
}
export function clearVerifiedSpecialists(domain?:VerifiedSpecialistDomain):void {
  if (!domain) {
    localStorage.removeItem(VERIFIED_SPECIALISTS_KEY);
    localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,'false');
    return;
  }
  const saved=storedSummaries(Date.now());
  const flags=storedFlags();
  delete saved.domains[domain];
  flags.domains[domain]=false;
  localStorage.setItem(VERIFIED_SPECIALISTS_KEY,JSON.stringify(saved));
  localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,JSON.stringify(flags));
}
/**
 * No newly proposed agents, no privilege changes, no cross-domain evidence.
 * These results determine only which of the already eligible supplemental
 * reviewer roles is used for that same domain.
 */
export function chooseVerifiedSpecialist(
  domain:string,options:readonly string[],
  summary:VerifiedSpecialistSummary|null=
    domainValid(domain)?loadVerifiedSpecialists(domain):null,
  enabled=domainValid(domain)?verifiedSpecialistRoutingEnabled(domain):false,
):string|null {
  if (!domainValid(domain) || !enabled || !summary || summary.domain!==domain)return null;
  const verified=new Map(summary.ranking.filter(r=>r.attempts>=3).map(r=>[r.specialist,r]));
  const eligible=options.filter(option=>verified.has(option));
  if (eligible.length<2)return null;
  eligible.sort((a,b)=>{
    const left=verified.get(a)!,right=verified.get(b)!;
    return right.accuracy-left.accuracy ||
      (left.averageVerifiedMs??Infinity)-(right.averageVerifiedMs??Infinity) ||
      options.indexOf(a)-options.indexOf(b);
  });
  return eligible[0];
}
