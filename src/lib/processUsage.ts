/**
 * Frontend client for the per-process resource ledger (`process_commands.rs`'s
 * `process_usage_ledger`, over `process_table.rs`).
 *
 * The whole point of this module is one rule, enforced in one place:
 *
 * **`null` means UNAVAILABLE. It never means zero.**
 *
 * Rust guarantees the stronger half of that — a row cannot be written with a
 * gap that has no stated reason (`ProcessUsage::new`), so every `null` here
 * comes with an entry in `usage.unavailable` naming the field and why. What the
 * frontend has to guarantee is the other half: that a `null` is never
 * formatted, summed, or charted as `0`. TypeScript cannot express "you must
 * branch on this", so {@link renderMeasurement} is the single chokepoint that
 * does: it returns a tagged union, and there is no code path through it that
 * turns an unmeasured field into a number.
 *
 * Read-only and fail-soft, matching `processTable.ts`: the ledger is an
 * observability surface, so a failed read is a worse panel, never a worse turn.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";

import { formatBytes, formatDuration } from "./format";
import type { ProcessExitStatus, ProcessKind, ProcessState } from "./processTable";

/** `{field, reason}`, the same vocabulary `runtime_telemetry.rs` uses. */
export interface UsageFieldNote {
  field: string;
  reason: string;
}

/**
 * The nine measurements, flattened into `usage` by serde (`#[serde(flatten)]`
 * on `ProcessUsage::measured`) alongside the notes that explain the gaps.
 */
export interface ProcessUsageMeasurements {
  cpuTimeMs: number | null;
  peakRssBytes: number | null;
  bytesRead: number | null;
  bytesWritten: number | null;
  bytesEgressed: number | null;
  tokensIn: number | null;
  tokensOut: number | null;
  gpuResidentBytes: number | null;
  gpuDeviceMs: number | null;
  unavailable: UsageFieldNote[];
}

export interface ProcessUsageRow {
  processId: string;
  kind: ProcessKind;
  externalId: string;
  runId: string | null;
  workspace: string | null;
  state: ProcessState;
  exitStatus: ProcessExitStatus | null;
  /**
   * Derived from the row's timestamps, so it sits beside `usage` rather than
   * inside it — but its gap is explained inside `usage.unavailable` under the
   * `wallTimeMs` field name like any other. That is why {@link measurementOf}
   * reads the value and the reason from two different places.
   */
  wallTimeMs: number | null;
  usage: ProcessUsageMeasurements;
}

/**
 * One field's total, with how many rows could and could not contribute.
 *
 * `value: null` when no row measured the field — the total of nothing is
 * unknown, not zero. A non-null total with `unavailableRows > 0` is answering a
 * narrower question than it looks like, which is exactly what the UI must say.
 */
export interface ProcessUsageTotal {
  value: number | null;
  measuredRows: number;
  unavailableRows: number;
}

export interface ProcessUsageAggregate {
  rows: number;
  wallTimeMs: ProcessUsageTotal;
  cpuTimeMs: ProcessUsageTotal;
  bytesRead: ProcessUsageTotal;
  bytesWritten: ProcessUsageTotal;
  bytesEgressed: ProcessUsageTotal;
  tokensIn: ProcessUsageTotal;
  tokensOut: ProcessUsageTotal;
  gpuDeviceMs: ProcessUsageTotal;
  /** A maximum, not a sum — adding two peaks invents a moment nothing observed. */
  peakRssBytes: ProcessUsageTotal;
  gpuResidentBytes: ProcessUsageTotal;
}

/** One host a process's *allowed* egress reached, and how often. */
export interface EgressDestination {
  scheme: string;
  host: string;
  port: number;
  requests: number;
  firstSeenMs: number;
  lastSeenMs: number;
}

/**
 * Where one process's egress went.
 *
 * `dropped` is the count of requests to destinations past the recorder's cap
 * (`run_scope::MAX_DESTINATIONS`). It is shown rather than hidden because a
 * truncated list that does not say it is truncated reads as a complete one —
 * the same rule the unavailable-measurement branches follow.
 */
export interface ProcessEgressDestinations {
  destinations: EgressDestination[];
  dropped: number;
}

export interface ProcessUsageLedger {
  rows: ProcessUsageRow[];
  totals: ProcessUsageAggregate;
  /**
   * Keyed by `processId`, and only for processes that reached somewhere — a
   * missing key means nothing was recorded, which is why callers read it with
   * {@link destinationsFor} rather than indexing it directly.
   */
  destinations: Record<string, ProcessEgressDestinations>;
}

/**
 * The destinations recorded for one process, or `null` when none were.
 *
 * `null` rather than an empty list on purpose: "this process reached nowhere"
 * and "this build recorded nothing" are the same absence here, and a surface
 * that rendered an empty list would be claiming the first.
 */
export function destinationsFor(
  ledger: Pick<ProcessUsageLedger, "destinations"> | null,
  processId: string,
): ProcessEgressDestinations | null {
  const found = ledger?.destinations?.[processId];
  if (!found) return null;
  return found.destinations.length > 0 || found.dropped > 0 ? found : null;
}

export interface ProcessUsageFilter {
  processId?: string;
  runId?: string;
  workspace?: string;
  /** Only exited processes, whose ledger rows are closed out. */
  closedOnly?: boolean;
  limit?: number;
}

/** How the aggregate folded a field, so a total can say which it is. */
export type UsageFold = "sum" | "max";

export type UsageUnit = "duration" | "bytes" | "count";

/** Every aggregate key that is a total — `rows` is a count, not a measurement. */
export type UsageTotalField = Exclude<keyof ProcessUsageAggregate, "rows">;

export interface UsageFieldSpec {
  field: UsageTotalField;
  unit: UsageUnit;
  fold: UsageFold;
}

/**
 * Every measurement, in display order, paired with how to format it and how the
 * aggregate folds it.
 *
 * The field names are the wire names verbatim (`process_usage.rs`'s `FIELD_*`
 * consts), because they are also the keys `usage.unavailable` notes use — a
 * paraphrase here would silently stop matching reasons to fields.
 */
export const USAGE_FIELDS: readonly UsageFieldSpec[] = [
  { field: "wallTimeMs", unit: "duration", fold: "sum" },
  { field: "cpuTimeMs", unit: "duration", fold: "sum" },
  { field: "peakRssBytes", unit: "bytes", fold: "max" },
  { field: "bytesRead", unit: "bytes", fold: "sum" },
  { field: "bytesWritten", unit: "bytes", fold: "sum" },
  { field: "bytesEgressed", unit: "bytes", fold: "sum" },
  { field: "tokensIn", unit: "count", fold: "sum" },
  { field: "tokensOut", unit: "count", fold: "sum" },
  { field: "gpuResidentBytes", unit: "bytes", fold: "max" },
  { field: "gpuDeviceMs", unit: "duration", fold: "sum" },
];

export interface UsageMeasurement {
  field: string;
  value: number | null;
  /** Why `value` is null. Non-null whenever the backend held its contract. */
  reason: string | null;
}

/** One field of one row, with its gap reason attached. */
export function measurementOf(row: ProcessUsageRow, field: string): UsageMeasurement {
  const value = field === "wallTimeMs"
    ? row.wallTimeMs
    : (row.usage as unknown as Record<string, number | null | undefined>)[field] ?? null;
  return {
    field,
    value,
    reason: value === null ? row.usage.unavailable.find((note) => note.field === field)?.reason ?? null : null,
  };
}

/**
 * Either a formatted number or an explained absence — never both, and never a
 * number standing in for an absence.
 *
 * A tagged union rather than a string so a caller cannot forget the second case:
 * there is no `.text` to read on an unavailable measurement.
 */
export type RenderedMeasurement =
  | { available: true; text: string }
  | { available: false; reason: string };

function formatUnit(value: number, unit: UsageUnit): string {
  if (unit === "bytes") return formatBytes(value, { fallback: "0 B" });
  if (unit === "duration") return formatDuration(value, { fallback: "0 ms", style: "precise" });
  return value.toLocaleString();
}

/**
 * The one place a measurement becomes display text.
 *
 * `unexplainedReason` covers the case the Rust side promises cannot happen — a
 * null with no note — because "unavailable, and we don't know why" is still an
 * honest rendering and a silent `0` is not. It is a translated string, so it is
 * passed in rather than hardcoded here.
 */
export function renderMeasurement(
  measurement: UsageMeasurement,
  unit: UsageUnit,
  unexplainedReason: string,
): RenderedMeasurement {
  if (measurement.value === null) {
    return { available: false, reason: measurement.reason ?? unexplainedReason };
  }
  return { available: true, text: formatUnit(measurement.value, unit) };
}

/** A total, through the same rule: an unmeasured total is not a zero total. */
export function renderTotal(
  total: ProcessUsageTotal,
  unit: UsageUnit,
  unmeasuredReason: string,
): RenderedMeasurement {
  if (total.value === null) return { available: false, reason: unmeasuredReason };
  return { available: true, text: formatUnit(total.value, unit) };
}

/**
 * Whether a total leaves rows out — the caller must then say so beside it.
 *
 * Separate from {@link renderTotal} on purpose: a partial total is still a real
 * number and should be shown, just never alone.
 */
export function totalIsPartial(total: ProcessUsageTotal): boolean {
  return total.unavailableRows > 0;
}

export async function processUsageLedger(filter?: ProcessUsageFilter): Promise<ProcessUsageLedger | null> {
  if (!isTauri()) return null;
  return invoke<ProcessUsageLedger>("process_usage_ledger", { args: filter ?? {} });
}
