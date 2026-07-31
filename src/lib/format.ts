/**
 * Shared display formatters.
 *
 * These existed as ~20 near-identical private copies across panels
 * (`formatBytes` ×8, `formatDuration` ×6, `formatTime` ×5), which is how the
 * same numbers ended up rendered three different ways on three screens. Each
 * function below takes the options its former copies actually differed on —
 * the empty-value placeholder and, for durations, whether to prefer a
 * compact `1h 5m` form — so a call site keeps its exact previous output
 * while sharing one implementation.
 */

const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;

export interface FormatOptions {
  /** Rendered for null/undefined/non-finite input. Panels that never show a
   * placeholder pass `"0 B"`/`"0 ms"`; most show an em dash. */
  fallback?: string;
}

/**
 * Human-readable byte size. Precision follows magnitude (bytes are whole,
 * small values get 2 decimals, large values 1), which is what every previous
 * copy converged on independently.
 */
export function formatBytes(
  value: number | null | undefined,
  { fallback = "—" }: FormatOptions = {},
): string {
  if (value == null || !Number.isFinite(value)) return fallback;
  if (value <= 0) return "0 B";
  const exponent = Math.min(
    Math.floor(Math.log(value) / Math.log(1024)),
    BYTE_UNITS.length - 1,
  );
  const scaled = value / 1024 ** exponent;
  const decimals = exponent === 0 ? 0 : scaled >= 10 ? 1 : 2;
  return `${scaled.toFixed(decimals)} ${BYTE_UNITS[exponent]}`;
}

export interface FormatDurationOptions extends FormatOptions {
  /**
   * `"precise"` (default) keeps sub-second resolution — `840 ms`, `2.4 s`,
   * `3m 5s` — for latency and per-call timings. `"coarse"` collapses to
   * `1h 5m` / `12m` / `45s` for accumulated totals where milliseconds are
   * noise.
   */
  style?: "precise" | "coarse";
}

export function formatDuration(
  value: number | null | undefined,
  { fallback = "—", style = "precise" }: FormatDurationOptions = {},
): string {
  if (value == null || !Number.isFinite(value) || value < 0) return fallback;
  if (style === "coarse") {
    if (value <= 0) return "0m";
    const totalSeconds = Math.round(value / 1_000);
    const hours = Math.floor(totalSeconds / 3_600);
    const minutes = Math.floor((totalSeconds % 3_600) / 60);
    const seconds = totalSeconds % 60;
    if (hours > 0) return `${hours}h ${minutes}m`;
    if (minutes > 0) return `${minutes}m`;
    return `${seconds}s`;
  }
  if (value < 1_000) return `${Math.round(value)} ms`;
  if (value < 60_000) return `${(value / 1_000).toFixed(value < 10_000 ? 1 : 0)} s`;
  return `${Math.floor(value / 60_000)}m ${Math.round((value % 60_000) / 1_000)}s`;
}

export interface FormatTimestampOptions extends FormatOptions {
  /** `"short"` (default) omits seconds; `"medium"` includes them, which run
   * timelines need to order same-minute events. */
  timeStyle?: "short" | "medium";
}

/**
 * Locale-aware absolute timestamp. Uses the user's locale and timezone
 * rather than a hardcoded format — these are read by a human on this
 * machine, never parsed.
 */
export function formatTimestamp(
  value: number | null | undefined,
  { fallback = "—", timeStyle = "short" }: FormatTimestampOptions = {},
): string {
  if (value == null || !Number.isFinite(value) || value <= 0) return fallback;
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle }).format(value);
}
