/**
 * Measured benchmark panel (`src-tauri/src/benchmark.rs`).
 *
 * ## No number is displayed that was not measured on this machine
 *
 * ROADMAP #2's rule is enforced by the type system here, not by convention.
 * Every metric on this panel is a {@link Measured}, and its unavailable branch
 * has no `text` property — so there is no compilable path that prints a number
 * for a field this run did not measure. The only branch available for a gap is
 * the one that renders the backend's own reason.
 *
 * There is deliberately **no chart**. A bar of height zero cannot say "unknown"
 * rather than "zero", which is exactly the lie this feature exists to prevent.
 * The per-repeat view is a table, where a cell can carry a sentence.
 */
import { useEffect, useMemo, useState, type FormEvent } from "react";
import { Gauge, Play } from "lucide-react";

import type {
  BenchmarkFieldNote,
  BenchmarkHistoryEntry,
  BenchmarkPeakMemory,
  BenchmarkRunResponse,
  BenchmarkSample,
  BenchmarkSpread,
} from "../../../lib/runtimeHubClient";
import { createM3OperationId, runtimeHubClient } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { StatusPill } from "../../ui";
import { BusyButton, CONTROL_CLASS, ErrorNotice, Field, SectionHeading } from "./RuntimeHubShared";
import { errorMessage } from "../../../lib/errors";
import { formatBytes } from "../../../lib/format";

/** Mirrors `benchmark::MIN_OUTPUT_TOKENS` and friends. The backend refuses
 * out-of-range input rather than clamping it, so these bounds exist only to
 * tell the user before a round trip — the value typed is the value sent. */
export const MIN_OUTPUT_TOKENS = 32;
export const MAX_OUTPUT_TOKENS = 2048;
export const MIN_REPEATS = 2;
export const MAX_REPEATS = 20;

/**
 * A metric that either was measured on this machine, or wasn't.
 *
 * The unavailable branch has no `text`: a component cannot print a number for
 * it, and the only thing it can render is `reason`.
 */
export type Measured = { measured: true; text: string } | { measured: false; reason: string };

/**
 * The reason a field is missing, taken from the backend's own notes.
 *
 * A `null` with no matching note is something Rust promises cannot happen, but
 * "unavailable, and the run did not say why" is still an honest sentence and a
 * silent `0` is not — so the fallback is a real sentence, never `""`. A note
 * whose reason is blank counts as no note for the same reason.
 */
export function unavailableReason(notes: BenchmarkFieldNote[], field: string): string {
  return (
    notes.find((note) => note.field === field && note.reason.trim())?.reason ??
    `this run reported no ${field} and recorded no reason for the gap`
  );
}

function formatMeasurement(value: number, unit: string): string {
  return `${value.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${unit}`;
}

/** One raw measurement: a number with its unit, or the note explaining the gap. */
export function measuredValue(
  value: number | null,
  notes: BenchmarkFieldNote[],
  field: string,
  unit: string,
): Measured {
  if (value === null) return { measured: false, reason: unavailableReason(notes, field) };
  return { measured: true, text: formatMeasurement(value, unit) };
}

/** Median with the min/max it sits between, or why there is no spread at all. */
export function measuredSpread(
  spread: BenchmarkSpread | null,
  notes: BenchmarkFieldNote[],
  field: string,
  unit: string,
): Measured {
  if (spread === null) return { measured: false, reason: unavailableReason(notes, field) };
  return {
    measured: true,
    text: `${formatMeasurement(spread.median, unit)} (min ${formatMeasurement(spread.min, unit)}, max ${formatMeasurement(spread.max, unit)}, n=${spread.n})`,
  };
}

/**
 * `stddev === null` is a single counted repeat. It renders as a reason, never as
 * `0` — zero would read as "perfectly repeatable" when nothing was compared.
 */
export function measuredStddev(spread: BenchmarkSpread): Measured {
  if (spread.stddev === null) {
    return {
      measured: false,
      reason: `a single repeat has no spread to report (n=${spread.n}), so no standard deviation exists`,
    };
  }
  return { measured: true, text: `± ${spread.stddev.toLocaleString(undefined, { maximumFractionDigits: 1 })}` };
}

/**
 * Peak memory, keeping this run's peak distinct from the runtime process's
 * lifetime high-water mark.
 *
 * `runPeakBytes` is set only when this run is what raised the mark, and that is
 * the one case the figure may be labelled as this run's peak. When the mark did
 * not rise, the lifetime value still *bounds* this run — so it is rendered as an
 * upper bound, with the backend's reason attached, and never as the run's peak.
 */
export function measuredPeakMemory(peak: BenchmarkPeakMemory): Measured {
  const reason = unavailableReason(peak.unavailable, "runPeakRssBytes");
  if (peak.runPeakBytes !== null) {
    return { measured: true, text: `${formatBytes(peak.runPeakBytes)} peak for this run` };
  }
  if (peak.processLifetimePeakBytes !== null) {
    return {
      measured: true,
      text: `at most ${formatBytes(peak.processLifetimePeakBytes)} — the runtime process's lifetime high-water mark, which this run did not raise, so it bounds this run's peak rather than reporting it (${reason})`,
    };
  }
  return { measured: false, reason };
}

/** A repeat's decode rate. An errored repeat renders its error, never `0 tok/s`. */
export function measuredRate(sample: BenchmarkSample): Measured {
  if (sample.timings.error !== null) return { measured: false, reason: sample.timings.error };
  return measuredValue(
    sample.decodeTokensPerSecond,
    sample.timings.unavailable,
    "decodeTokensPerSecond",
    "tok/s",
  );
}

/** The only place a {@link Measured} becomes pixels. */
function MeasuredText({ value }: { value: Measured }) {
  if (!value.measured) {
    return <span className="text-xs leading-snug text-warning">Unavailable: {value.reason}</span>;
  }
  return <span className="font-mono text-xs text-foreground">{value.text}</span>;
}

function Headline({ label, value, note }: { label: string; value: Measured; note?: Measured }) {
  return (
    <div className="min-w-0 rounded-lg border border-border bg-surface p-3">
      <p className="text-[11px] text-faint">{label}</p>
      <p className="mt-1">
        <MeasuredText value={value} />
      </p>
      {note && (
        <p className="mt-1">
          <MeasuredText value={note} />
        </p>
      )}
    </div>
  );
}

/**
 * Measurements kept from earlier sessions.
 *
 * A benchmark that is measured and then thrown away cannot inform anything, so
 * the backend persists each report with the machine it ran on. The machine is
 * what makes this safe to show: an entry measured somewhere else — or here
 * before the RAM, CPU count or accelerators changed — renders **what changed**
 * and no numbers at all, because "on the machine displaying it" is the whole
 * claim this surface makes.
 */
function SavedMeasurements({ entries }: { entries: BenchmarkHistoryEntry[] }) {
  if (!entries.length) return null;
  return (
    <section className="flex flex-col gap-3" aria-labelledby="runtime-hub-benchmark-history">
      <h3 id="runtime-hub-benchmark-history" className="text-sm font-semibold text-foreground">
        Saved measurements
      </h3>
      <div className="min-w-0 overflow-x-auto rounded-lg border border-border">
        <table className="w-full min-w-[40rem] text-left text-xs">
          <caption className="px-3 py-2 text-left text-[11px] text-faint">
            Most recent first, one per model per runtime. Re-running a pair replaces its entry rather than
            appending, since two reports for the same pair are not a time series.
          </caption>
          <thead className="bg-surface-2 text-faint">
            <tr>
              <th scope="col" className="px-3 py-2 font-medium">Model</th>
              <th scope="col" className="px-3 py-2 font-medium">Runtime</th>
              <th scope="col" className="px-3 py-2 font-medium">Time to first token</th>
              <th scope="col" className="px-3 py-2 font-medium">Decode rate</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((entry) => {
              const key = `${entry.report.runtimeId}/${entry.report.model}`;
              if (entry.freshness.state !== "thisMachine") {
                return (
                  <tr key={key} className="border-t border-border align-top">
                    <td className="px-3 py-2 text-foreground">{entry.report.model}</td>
                    <td className="px-3 py-2 text-muted">{entry.report.runtimeId}</td>
                    <td className="px-3 py-2 text-warning" colSpan={2}>
                      Measured on different hardware, so its numbers are not shown:{" "}
                      {entry.freshness.changed.join("; ")}. Re-run the benchmark to measure this machine.
                    </td>
                  </tr>
                );
              }
              return (
                <tr key={key} className="border-t border-border align-top">
                  <td className="px-3 py-2 text-foreground">{entry.report.model}</td>
                  <td className="px-3 py-2 text-muted">{entry.report.runtimeId}</td>
                  <td className="px-3 py-2">
                    <MeasuredText
                      value={measuredSpread(
                        entry.report.timeToFirstTokenMs,
                        entry.report.unavailable,
                        "timeToFirstTokenMs",
                        "ms",
                      )}
                    />
                  </td>
                  <td className="px-3 py-2">
                    <MeasuredText
                      value={measuredSpread(
                        entry.report.decodeTokensPerSecond,
                        entry.report.unavailable,
                        "decodeTokensPerSecond",
                        "tok/s",
                      )}
                    />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

export function RuntimeHubBenchmark() {
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const runtimeDetails = useRuntimeHubStore((state) => state.runtimeDetails);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);

  const inferRuntimes = useMemo(() => runtimes.filter((runtime) => runtime.canInfer), [runtimes]);
  const [runtimeId, setRuntimeId] = useState(() => inferRuntimes[0]?.descriptor.runtimeId ?? "");
  const [model, setModel] = useState("");
  const [maxOutputTokens, setMaxOutputTokens] = useState("128");
  const [repeats, setRepeats] = useState("5");
  const [busy, setBusy] = useState(false);
  const [backendError, setBackendError] = useState<string | null>(null);
  const [result, setResult] = useState<BenchmarkRunResponse | null>(null);
  const [history, setHistory] = useState<BenchmarkHistoryEntry[]>([]);

  // Loaded once on mount and refreshed after each run. A read failure leaves the
  // section absent rather than showing an error: the history is context for the
  // measurement, and failing to load it must not look like a failed benchmark.
  useEffect(() => {
    let cancelled = false;
    runtimeHubClient
      .benchmarkHistory()
      .then((entries) => {
        if (!cancelled) setHistory(entries);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [result]);

  const selectedRuntime = runtimeId || inferRuntimes[0]?.descriptor.runtimeId || "";
  // The runtime's own inventory when `refreshRuntime` has already loaded it
  // (the Runtimes tab populates it), falling back to the shared installed-model
  // list so the picker is never empty just because that tab was never opened.
  const inventory = runtimeDetails[selectedRuntime]?.inventory?.models ?? [];
  const modelOptions = useMemo(
    () =>
      inventory.length
        ? inventory.map((entry) => ({ value: entry.model_id, label: entry.display_name }))
        : installedModels.map((entry) => ({ value: entry.modelId, label: entry.displayName })),
    [installedModels, inventory],
  );

  const tokens = Number.parseInt(maxOutputTokens, 10);
  const repeatCount = Number.parseInt(repeats, 10);
  const localError = !selectedRuntime
    ? "No inference runtime is available to benchmark."
    : !model.trim()
      ? "Pick the model to measure."
      : !Number.isInteger(tokens) || tokens < MIN_OUTPUT_TOKENS || tokens > MAX_OUTPUT_TOKENS
        ? `Max output tokens must be between ${MIN_OUTPUT_TOKENS} and ${MAX_OUTPUT_TOKENS}. Below ${MIN_OUTPUT_TOKENS} the decode rate is noise rather than a measurement.`
        : !Number.isInteger(repeatCount) || repeatCount < MIN_REPEATS || repeatCount > MAX_REPEATS
          ? `Repeats must be between ${MIN_REPEATS} and ${MAX_REPEATS}. The first repeat is discarded as a warm-up, so ${MIN_REPEATS} is the floor that leaves anything counted.`
          : null;

  async function run(event: FormEvent) {
    event.preventDefault();
    if (localError) return;
    setBusy(true);
    setBackendError(null);
    try {
      // Sent exactly as typed: the backend refuses out-of-range input rather
      // than clamping, and rewriting it here would report a `maxOutputTokens`
      // that disagrees with what was asked for.
      const response = await runtimeHubClient.benchmarkRun(
        createM3OperationId("benchmark-run"),
        {
          runtimeId: selectedRuntime,
          model: model.trim(),
          prompt: null,
          maxOutputTokens: tokens,
          repeats: repeatCount,
        },
        repeatCount * 120_000,
      );
      setResult(response);
    } catch (error) {
      setResult(null);
      setBackendError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  const report = result?.report;
  const hardware = result?.hardware;

  return (
    <div
      role="tabpanel"
      id="runtime-hub-panel-benchmark"
      aria-labelledby="runtime-hub-tab-benchmark"
      className="flex flex-col gap-5"
    >
      <SectionHeading
        title="Measured benchmark"
        description="Times real generations on this machine: time-to-first-token, decode tokens/sec, and peak memory. Nothing here is estimated or carried over from another machine — a field this run could not measure shows the reason instead of a number, and there is no chart because a zero-height bar cannot say “unknown”."
      />

      <form onSubmit={run} className="flex flex-col gap-4 rounded-lg border border-border bg-background p-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Runtime">
            <select
              value={selectedRuntime}
              onChange={(event) => setRuntimeId(event.target.value)}
              className={CONTROL_CLASS}
              disabled={!inferRuntimes.length}
            >
              {!inferRuntimes.length && <option value="">No inference runtime available</option>}
              {inferRuntimes.map((runtime) => (
                <option key={runtime.descriptor.runtimeId} value={runtime.descriptor.runtimeId}>
                  {runtime.descriptor.label}
                </option>
              ))}
            </select>
          </Field>
          <Field
            label="Model"
            hint={inventory.length ? "From this runtime's own inventory." : "From the installed model list."}
          >
            <input
              list="runtime-hub-benchmark-models"
              value={model}
              onChange={(event) => setModel(event.target.value)}
              className={CONTROL_CLASS}
            />
            <datalist id="runtime-hub-benchmark-models">
              {modelOptions.map((entry) => (
                <option key={entry.value} value={entry.value}>
                  {entry.label}
                </option>
              ))}
            </datalist>
          </Field>
          <Field label="Max output tokens" hint={`${MIN_OUTPUT_TOKENS}–${MAX_OUTPUT_TOKENS} per repeat.`}>
            <input
              type="number"
              inputMode="numeric"
              min={MIN_OUTPUT_TOKENS}
              max={MAX_OUTPUT_TOKENS}
              value={maxOutputTokens}
              onChange={(event) => setMaxOutputTokens(event.target.value)}
              className={CONTROL_CLASS}
            />
          </Field>
          <Field
            label="Repeats"
            hint={`${MIN_REPEATS}–${MAX_REPEATS}. The first repeat is a warm-up and is excluded from every statistic.`}
          >
            <input
              type="number"
              inputMode="numeric"
              min={MIN_REPEATS}
              max={MAX_REPEATS}
              value={repeats}
              onChange={(event) => setRepeats(event.target.value)}
              className={CONTROL_CLASS}
            />
          </Field>
        </div>

        <ErrorNotice message={backendError} />
        {localError && !busy && <p className="text-xs leading-5 text-muted">{localError}</p>}

        <div className="flex justify-end">
          <BusyButton type="submit" variant="primary" busy={busy} disabled={Boolean(localError)}>
            <Play size={15} aria-hidden="true" /> Run benchmark
          </BusyButton>
        </div>
      </form>

      <SavedMeasurements entries={history} />

      {report && hardware && (
        <section className="flex flex-col gap-4" aria-labelledby="runtime-hub-benchmark-results">
          <div className="flex flex-wrap items-center gap-2">
            <Gauge size={16} className="text-muted" aria-hidden="true" />
            <h3 id="runtime-hub-benchmark-results" className="text-sm font-semibold text-foreground">
              {report.model} on {report.runtimeId}
            </h3>
            <StatusPill tone="neutral">{report.maxOutputTokens} tokens/repeat</StatusPill>
            <StatusPill tone="warning">
              {report.warmupDiscarded} of {report.repeatsRequested} repeats discarded as warm-up
            </StatusPill>
          </div>

          <p className="text-xs leading-5 text-muted">
            Measured on this machine: {formatBytes(hardware.total_ram_bytes)} RAM,{" "}
            {hardware.logical_cpu_count} logical CPUs, {hardware.platform.os}/{hardware.platform.arch}.
          </p>

          <p className="text-xs leading-5 text-warning">
            The first {report.warmupDiscarded} repeat
            {report.warmupDiscarded === 1 ? "" : "s"} below {report.warmupDiscarded === 1 ? "is" : "are"} marked
            warm-up and excluded from the medians, min/max and standard deviations — a cold first request pays for
            loading weights, and charging that to time-to-first-token would report load time as prefill.
          </p>

          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <Headline
              label="Time to first token (median)"
              value={measuredSpread(report.timeToFirstTokenMs, report.unavailable, "timeToFirstTokenMs", "ms")}
              note={report.timeToFirstTokenMs ? measuredStddev(report.timeToFirstTokenMs) : undefined}
            />
            <Headline
              label="Decode rate (median)"
              value={measuredSpread(
                report.decodeTokensPerSecond,
                report.unavailable,
                "decodeTokensPerSecond",
                "tok/s",
              )}
              note={report.decodeTokensPerSecond ? measuredStddev(report.decodeTokensPerSecond) : undefined}
            />
            <Headline label="Peak memory" value={measuredPeakMemory(report.peakMemory)} />
          </div>

          <div className="min-w-0 overflow-x-auto rounded-lg border border-border">
            <table className="w-full min-w-[44rem] text-left text-xs">
              <caption className="px-3 py-2 text-left text-[11px] text-faint">
                Every repeat in execution order. Warm-up repeats are shown but never counted.
              </caption>
              <thead className="bg-surface-2 text-faint">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">Repeat</th>
                  <th scope="col" className="px-3 py-2 font-medium">Time to first token</th>
                  <th scope="col" className="px-3 py-2 font-medium">Decode time</th>
                  <th scope="col" className="px-3 py-2 font-medium">Output tokens</th>
                  <th scope="col" className="px-3 py-2 font-medium">Decode rate</th>
                </tr>
              </thead>
              <tbody>
                {report.samples.map((sample) => (
                  <tr key={sample.repeat} className="border-t border-border align-top">
                    <th scope="row" className="px-3 py-2 font-medium text-foreground">
                      {sample.repeat}
                      {sample.warmup && (
                        <span className="ml-1.5 rounded bg-warning-soft px-1.5 py-0.5 text-[10px] font-medium text-warning">
                          warm-up · excluded
                        </span>
                      )}
                    </th>
                    <td className="px-3 py-2">
                      <MeasuredText
                        value={measuredValue(
                          sample.timings.timeToFirstTokenMs,
                          sample.timings.unavailable,
                          "timeToFirstTokenMs",
                          "ms",
                        )}
                      />
                    </td>
                    <td className="px-3 py-2">
                      <MeasuredText
                        value={measuredValue(sample.timings.decodeMs, sample.timings.unavailable, "decodeMs", "ms")}
                      />
                    </td>
                    <td className="px-3 py-2">
                      <MeasuredText
                        value={measuredValue(
                          sample.timings.outputTokens,
                          sample.timings.unavailable,
                          "outputTokens",
                          "tokens",
                        )}
                      />
                    </td>
                    <td className="px-3 py-2">
                      <MeasuredText value={measuredRate(sample)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {report.unavailable.length > 0 && (
            <div>
              <p className="mb-1.5 text-xs font-medium text-muted">Fields this run could not measure</p>
              <ul className="space-y-0.5 text-xs leading-5 text-warning">
                {report.unavailable.map((note, index) => (
                  <li key={`${note.field}-${index}`}>
                    <span className="font-mono">{note.field}</span>: {note.reason}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </section>
      )}
    </div>
  );
}

export default RuntimeHubBenchmark;
