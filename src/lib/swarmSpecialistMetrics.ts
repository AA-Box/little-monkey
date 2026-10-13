/**
 * Small bounded pool of historical specialist completion outcomes.
 * Measures execution reliability and wall duration, NOT answer quality.
 * No prompts, source files, reports or model credentials are retained.
 */
import { chooseVerifiedSpecialist } from './swarmVerifiedSpecialists';

export type SpecialistDomain = 'security' | 'frontend' | 'backend' | 'performance' | 'general';
export interface SpecialistStats { successes: number; failures: number; completedMs: number }
export type SpecialistMetrics = Record<string, SpecialistStats>;
export const SPECIALIST_METRICS_KEY = 'little-monkey-swarm-specialists-v1';
const MAX_PROFILES = 48;
function key(domain: SpecialistDomain, specialist: string): string {
  if (!specialist.trim() || specialist.length > 120 || /[\x00-\x1f]/.test(specialist)) throw new Error('Invalid specialist label.');
  return domain + ':' + specialist;
}
export function loadSpecialistMetrics(): SpecialistMetrics {
  try {
    const raw = localStorage.getItem(SPECIALIST_METRICS_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    const output: SpecialistMetrics = {};
    for (const [id, value] of Object.entries(parsed).slice(0, MAX_PROFILES)) {
      if (!/^(security|frontend|backend|performance|general):[^\x00-\x1f]{1,120}$/.test(id)) continue;
      const v = value as Partial<SpecialistStats> | null;
      if (!v || !Number.isSafeInteger(v.successes) || !Number.isSafeInteger(v.failures) ||
          (v.successes ?? -1) < 0 || (v.failures ?? -1) < 0 ||
          (v.successes ?? 0) > 1_000 || (v.failures ?? 0) > 1_000 ||
          !Number.isFinite(v.completedMs) || (v.completedMs ?? -1) < 0 ||
          (v.completedMs ?? 0) > 1_000_000_000) continue;
      output[id] = { successes: v.successes!, failures: v.failures!, completedMs: v.completedMs! };
    }
    return output;
  } catch { return {}; }
}
export function recordSpecialistMetric(domain: SpecialistDomain, specialist: string, success: boolean, elapsedMs: number) {
  if (!Number.isFinite(elapsedMs) || elapsedMs < 0) return;
  try {
    const stats = loadSpecialistMetrics();
    const id = key(domain, specialist);
    const previous = stats[id] ?? { successes: 0, failures: 0, completedMs: 0 };
    const scale = previous.successes + previous.failures >= 1_000 ? 0.5 : 1;
    stats[id] = {
      successes: Math.floor(previous.successes * scale) + Number(success),
      failures: Math.floor(previous.failures * scale) + Number(!success),
      completedMs: previous.completedMs * scale + (success ? Math.min(elapsedMs, 1_800_000) : 0),
    };
    const subset = Object.entries(stats).slice(-MAX_PROFILES);
    localStorage.setItem(SPECIALIST_METRICS_KEY, JSON.stringify(Object.fromEntries(subset)));
  } catch { /* telemetry cannot change a task's outcome */ }
}

/** Select one supplemental specialist based on observed success/time. Under
 * three observations per candidate, prioritize less-tried experts so the
 * choice can actually adapt instead of permanently locking to the first. */
export function chooseSupplementalSpecialist(
  domain: SpecialistDomain,
  options: readonly string[],
  metrics = loadSpecialistMetrics(),
): string {
  if (!options.length) throw new Error('Specialist pool is empty.');
  const verified = chooseVerifiedSpecialist(domain, options);
  if (verified) return verified; // opt-in source-verified accuracy, never completion telemetry
  return [...options].sort((a, b) => {
    const aa = metrics[key(domain, a)] ?? { successes: 0, failures: 0, completedMs: 0 };
    const bb = metrics[key(domain, b)] ?? { successes: 0, failures: 0, completedMs: 0 };
    const countA = aa.successes + aa.failures, countB = bb.successes + bb.failures;
    if (countA < 3 || countB < 3) {
      if (countA !== countB) return countA - countB;
      return options.indexOf(a) - options.indexOf(b);
    }
    const scoreA = (aa.successes + 1) / (countA + 2);
    const scoreB = (bb.successes + 1) / (countB + 2);
    if (scoreA !== scoreB) return scoreB - scoreA;
    const timeA = aa.successes ? aa.completedMs / aa.successes : Infinity;
    const timeB = bb.successes ? bb.completedMs / bb.successes : Infinity;
    return timeA - timeB || options.indexOf(a) - options.indexOf(b);
  })[0];
}
