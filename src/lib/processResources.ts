/**
 * Turning one process's resource report into the words a person reads.
 *
 * Pure, and deliberately thin: every *judgement* — what the class declares, what
 * is installed, who tightened it, which mechanism holds it — is made in Rust and
 * arrives typed on {@link ProcessLimitReport}. What is left here is presentation:
 * bytes into "512 MiB", and the two typed answers into the single enforcement
 * word a row shows.
 *
 * The split matters because the alternative has already happened elsewhere in
 * this codebase: a mechanism table maintained in TypeScript beside the one doing
 * the enforcing drifts, and then the panel and `monkey processes` disagree about
 * what is bounding the same process. Nothing here invents a mechanism name, a
 * level, or a limit that the backend did not state.
 */
import type { LimitBreach, ProcessLimitReport } from "./processTable";

/**
 * How a resource is held, as one word.
 *
 * Five values, and the last two are not degrees of the third:
 *
 * - `kernel` — the bound survives this app dying.
 * - `supervised` — a sampler in this app measures and acts; it dies with the app.
 * - `owner-sourced` — a real bound whose number comes from a recipe, a workflow
 *   definition or a session's own settings rather than from this row.
 * - `unavailable` — nothing holds it here, and the report says what is missing.
 * - `not-applicable` — the resource is not a question for this workload, which is
 *   different from a missing mechanism.
 */
export type EnforcementLevel =
  | "kernel"
  | "supervised"
  | "owner-sourced"
  | "unavailable"
  | "not-applicable";

/**
 * The one enforcement word for a resource, preferring the host's answer.
 *
 * Two typed answers arrive per limit and they answer different questions. The
 * static one (`supportStatus`) is the contract: does anything read this field for
 * this kind, on any machine. The host one (`host`) is this machine right now: a
 * Linux box with a delegated cgroup holds a shell's memory in the kernel and one
 * without falls back to a supervisor, and both are `enforced` statically.
 *
 * The host answer wins where there is one, because it is the more specific truth
 * and it is the one a user is asking about when they look at a running process.
 */
export function enforcementOf(report: ProcessLimitReport): EnforcementLevel {
  if (report.host) {
    switch (report.host.status) {
      case "enforced":
        return report.host.level;
      case "unavailable":
        return "unavailable";
      case "not_applicable":
        return "not-applicable";
    }
  }
  switch (report.supportStatus) {
    case "enforced":
      // No host answer means this kind owns no OS process tree, so whatever
      // enforces the field is in-app rather than in the kernel.
      return "supervised";
    case "owner-sourced":
      return "owner-sourced";
    default:
      return "unavailable";
  }
}

/** Whichever detail explains this limit's enforcement, host answer first. */
export function enforcementDetail(report: ProcessLimitReport): string {
  if (report.host) {
    return report.host.status === "enforced" ? report.host.mechanism : report.host.reason;
  }
  return report.supportDetail;
}

/** Whether this resource is bounded at all for this process. */
export function isBounded(report: ProcessLimitReport): boolean {
  return report.effective !== null && report.effective !== undefined;
}

const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB"] as const;

/**
 * Binary units, because every number here is one: a cgroup's `memory.max`, a job
 * object's `JobMemoryLimit` and an output cap are all powers of two, and
 * rendering 4 GiB as "4.29 GB" would make the panel disagree with the value the
 * kernel was given.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = unit === 0 ? value : Math.round(value * 100) / 100;
  return `${rounded} ${BYTE_UNITS[unit]}`;
}

/** Milliseconds as the coarsest unit that keeps the number readable. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${Math.round(seconds * 10) / 10} s`;
  const minutes = seconds / 60;
  if (minutes < 60) return `${Math.round(minutes * 10) / 10} min`;
  return `${Math.round((minutes / 60) * 10) / 10} h`;
}

/**
 * A limit's value in the unit that limit is measured in.
 *
 * Keyed off the backend's own field name rather than a parallel enum, so a new
 * `ProcessLimits` field shows up here as its raw number — visibly unformatted —
 * instead of being silently rendered in the wrong unit.
 */
export function formatLimitValue(limit: string, value: number): string {
  switch (limit) {
    case "max_wall_ms":
      return formatDuration(value);
    case "max_memory_bytes":
    case "max_output_bytes":
      return formatBytes(value);
    default:
      return String(value);
  }
}

/**
 * Whether a breach's two numbers being equal is expected rather than wrong.
 *
 * A kernel-held bound exists so the workload never passes the number: a cgroup
 * refuses the thirteenth fork and leaves `pids.current` at twelve. So `observed
 * === configured` is the *normal* shape of a kernel breach, and a UI that only
 * knew "observed exceeded configured" would render the limit that worked best as
 * the one that looks like it did not fire. The evidence string is what says which
 * counter proved it, and it is present exactly when this is true.
 */
export function breachHeldAtTheCap(breach: LimitBreach): boolean {
  return breach.level === "kernel" && breach.observed <= breach.configured;
}
