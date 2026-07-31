/**
 * Client-side request-rate tracking, purely to warn against a cap the user
 * enters themselves (`settingsStore`'s `providerRateLimits`) — this app
 * never asserts what a provider's actual free-tier limit is, since those
 * change and aren't reliably knowable from here.
 */
import type { ProviderRateLimit } from '../store/settingsStore';

const STORAGE_KEY = 'little-monkey-rate-limit-log';
const ONE_MINUTE_MS = 60_000;
const ONE_DAY_MS = 24 * 60 * ONE_MINUTE_MS;
const WARNING_THRESHOLD = 0.8;
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

export type RateLimitWarningWindow = 'minute' | 'day';
export type RateLimitWarningSeverity = 'approaching' | 'exceeded';

export interface RateLimitWarning {
  providerId: string;
  window: RateLimitWarningWindow;
  severity: RateLimitWarningSeverity;
  currentCount: number;
  nextCount: number;
  limit: number;
  percent: number;
}

function configuredLimit(value: number | undefined): number | null {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) return null;
  return Math.max(1, Math.trunc(value));
}

function evaluateWindow(
  providerId: string,
  window: RateLimitWarningWindow,
  currentCount: number,
  rawLimit: number | undefined,
): RateLimitWarning | null {
  const limit = configuredLimit(rawLimit);
  if (limit === null) return null;
  const nextCount = currentCount + 1;
  const percent = nextCount / limit;
  if (percent < WARNING_THRESHOLD) return null;
  return {
    providerId,
    window,
    severity: nextCount > limit ? 'exceeded' : 'approaching',
    currentCount,
    nextCount,
    limit,
    percent,
  };
}

/**
 * Evaluates the request that is about to be attempted. Counts are
 * intentionally read before `attemptStream` calls `recordRequest`, so the
 * returned `nextCount` is the number the imminent request will consume.
 * Failed requests and provider failovers still count because the provider
 * received an attempt.
 */
export function evaluateRateLimit(
  providerId: string,
  configured: ProviderRateLimit | undefined,
  now: number = Date.now(),
): RateLimitWarning[] {
  if (!configured) return [];
  return [
    evaluateWindow(
      providerId,
      'minute',
      getCountLastMinute(providerId, now),
      configured.rpm,
    ),
    evaluateWindow(
      providerId,
      'day',
      getCountLastDay(providerId, now),
      configured.rpd,
    ),
  ].filter((warning): warning is RateLimitWarning => warning !== null);
}
