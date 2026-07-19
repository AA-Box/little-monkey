/**
 * Client-side request-rate tracking, purely to warn against a cap the user
 * enters themselves (`settingsStore`'s `providerRateLimits`) — this app
 * never asserts what a provider's actual free-tier limit is, since those
 * change and aren't reliably knowable from here.
 */
const STORAGE_KEY = 'little-monkey-rate-limit-log';
const ONE_MINUTE_MS = 60_000;
const ONE_DAY_MS = 24 * 60 * ONE_MINUTE_MS;
/** Timestamps older than this are pruned on every read/write — nothing configured warns past a day, so nothing needs to be kept longer. */
const MAX_AGE_MS = ONE_DAY_MS;

type Log = Record<string, number[]>;

function readLog(): Log {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== 'object') return {};
    return parsed as Log;
  } catch {
    return {};
  }
}

function writeLog(log: Log): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(log));
  } catch {
    // Ignore — this tracker is a best-effort UI hint, never load-bearing.
  }
}

function prune(timestamps: number[], now: number): number[] {
  return timestamps.filter((ts) => now - ts <= MAX_AGE_MS);
}

/** Records one request against `providerId` right now. Call once per attempt, regardless of success/failure — a failed attempt still counts against a provider's rate limit. */
export function recordRequest(providerId: string, now: number = Date.now()): void {
  const log = readLog();
  const pruned = prune(log[providerId] ?? [], now);
  pruned.push(now);
  log[providerId] = pruned;
  writeLog(log);
}

/** Count of requests recorded for `providerId` within the last `windowMs`. */
export function getCountInWindow(providerId: string, windowMs: number, now: number = Date.now()): number {
  const log = readLog();
  const pruned = prune(log[providerId] ?? [], now);
  return pruned.filter((ts) => now - ts <= windowMs).length;
}

/** Convenience: count within the last minute. */
export function getCountLastMinute(providerId: string, now: number = Date.now()): number {
  return getCountInWindow(providerId, ONE_MINUTE_MS, now);
}

/** Convenience: count within the last 24h. */
export function getCountLastDay(providerId: string, now: number = Date.now()): number {
  return getCountInWindow(providerId, ONE_DAY_MS, now);
}
