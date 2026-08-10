import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { DecisionRow, UsageRow, UsageTotalCard } from "./ResourceLedgerPanel";
import { USAGE_FIELDS, type ProcessUsageAggregate, type ProcessUsageRow } from "../../lib/processUsage";
import type { SchedulerDecision } from "../../lib/daemonClient";

/**
 * Rendered per row rather than through the whole panel: zustand v5 serves
 * `getInitialState()` as its SSR snapshot, so a store seeded with `setState`
 * is invisible to `renderToStaticMarkup`. The rows are where the honesty rules
 * live anyway — they take their data as props, so what is asserted here is
 * exactly what a user sees.
 */

function usageTotal(value: number | null, measuredRows: number, unavailableRows: number) {
  return { value, measuredRows, unavailableRows };
}

const row: ProcessUsageRow = {
  processId: "proc-1",
  kind: "chat_turn",
  externalId: "turn-abc",
  runId: "run-1",
  workspace: "/workspace/repo",
  state: "exited",
  exitStatus: "succeeded",
  wallTimeMs: 4_000,
  usage: {
    cpuTimeMs: 1_200,
    peakRssBytes: 64 * 1024 * 1024,
    bytesRead: 2_048,
    bytesWritten: 512,
    bytesEgressed: null,
    tokensIn: 900,
    tokensOut: 120,
    gpuResidentBytes: null,
    gpuDeviceMs: null,
    unavailable: [
      { field: "bytesEgressed", reason: "nothing fed the ledger a byte count" },
      { field: "gpuResidentBytes", reason: "no runtime in this build reports GPU residency" },
      { field: "gpuDeviceMs", reason: "no runtime in this build reports GPU device time" },
    ],
  },
};

const totals: ProcessUsageAggregate = {
  rows: 10,
  wallTimeMs: usageTotal(40_000, 10, 0),
  cpuTimeMs: usageTotal(9_000, 3, 7),
  bytesRead: usageTotal(4_096, 10, 0),
  bytesWritten: usageTotal(1_024, 10, 0),
  bytesEgressed: usageTotal(null, 0, 10),
  tokensIn: usageTotal(1_800, 10, 0),
  tokensOut: usageTotal(240, 10, 0),
  gpuDeviceMs: usageTotal(null, 0, 10),
  peakRssBytes: usageTotal(64 * 1024 * 1024, 10, 0),
  gpuResidentBytes: usageTotal(null, 0, 10),
};

const decision: SchedulerDecision = {
  decidedAtMs: Date.UTC(2026, 6, 1, 15, 4, 9),
  jobId: "job-chosen",
  outcome: "admitted",
  processClass: "batch",
  effectiveClass: "interactive",
  workspace: "/workspace/repo",
  passedOver: ["job-second", "job-third"],
  detail: "admitted with 9.2 GiB available",
  measurement: "available_memory_bytes",
  measuredValue: 9_878_000_000,
  // Deliberately five seconds before the decision: the cited reading is older
  // than the row citing it, which is the distinction the UI must preserve.
  measuredAtMs: Date.UTC(2026, 6, 1, 15, 4, 4),
};

function spec(field: string) {
  const found = USAGE_FIELDS.find((entry) => entry.field === field);
  if (!found) throw new Error(`no field spec for ${field}`);
  return found;
}

describe("resource ledger rows", () => {
  it("renders a null measurement as unavailable with its reason and never as zero", () => {
    const markup = renderToStaticMarkup(<UsageRow row={row} />);

    expect(markup).toContain("Bytes egressed");
    expect(markup).toContain("unavailable — nothing fed the ledger a byte count");
    expect(markup).toContain("unavailable — no runtime in this build reports GPU residency");
    expect(markup).toContain("unavailable — no runtime in this build reports GPU device time");
    // Measured fields still render their real numbers.
    expect(markup).toContain("64.0 MB");
    expect(markup).toContain("2.00 KB");
    expect(markup).toContain("512 B");
    // And the unmeasured ones never stand in as zeros.
    expect(markup).not.toContain("0 B<");
    expect(markup).not.toContain("0 ms");
    expect(markup).toContain("3 of 10 measurements unavailable");
  });

  it("renders a measured zero as zero", () => {
    // The inverse of the rule, and why it matters: a real 0 is a fact, and
    // hiding it behind "unavailable" would be the same failure in reverse.
    const measured: ProcessUsageRow = {
      ...row,
      usage: {
        ...row.usage,
        bytesEgressed: 0,
        unavailable: row.usage.unavailable.filter((note) => note.field !== "bytesEgressed"),
      },
    };
    const markup = renderToStaticMarkup(<UsageRow row={measured} />);
    expect(markup).toContain("0 B");
    expect(markup).not.toContain("unavailable — nothing fed the ledger a byte count");
  });

  it("shows the runtime's measured prompt-cache reuse beside the tokens it came from", () => {
    const markup = renderToStaticMarkup(
      <UsageRow row={row} contextReuse={{ reusedTokens: 9, evaluatedTokens: 1_001 }} />,
    );
    expect(markup).toContain("Prompt cache, as the runtime measured it");
    expect(markup).toContain("0.9% reused");
    // The denominator is on screen too: a percentage nobody can check against a
    // token count is not a measurement a reader can trust.
    expect(markup).toContain("9 tokens saved, 1001 evaluated");
  });

  it("says nothing at all when the runtime reported no reuse figure", () => {
    // Ollama and MLX report nothing. A 0% on this row would be this app claiming
    // a measurement no runtime made.
    const markup = renderToStaticMarkup(<UsageRow row={row} />);
    expect(markup).not.toContain("Prompt cache");
    expect(markup).not.toContain("reused");
  });
});

describe("resource ledger totals", () => {
  it("reports a partial total beside the rows it could not read", () => {
    const markup = renderToStaticMarkup(<UsageTotalCard totals={totals} spec={spec("cpuTimeMs")} />);
    // The number is shown — it is real — but never alone.
    expect(markup).toContain("9.0 s");
    expect(markup).toContain("measured on 3 of 10 rows");
  });

  it("reports a total nothing measured as having no total, not as zero", () => {
    const markup = renderToStaticMarkup(<UsageTotalCard totals={totals} spec={spec("bytesEgressed")} />);
    expect(markup).toContain("no row measured this, so there is no total");
    expect(markup).toContain("measured on 0 of 10 rows");
    expect(markup).not.toContain("0 B");
  });

  it("labels a peak as a peak rather than a sum", () => {
    // Adding two processes' peak footprints invents a moment nothing observed.
    const markup = renderToStaticMarkup(<UsageTotalCard totals={totals} spec={spec("peakRssBytes")} />);
    expect(markup).toContain("peak, not a sum");
    expect(markup).toContain("measured on 10 of 10 rows");
  });

  it("still shows full coverage on a complete total", () => {
    const markup = renderToStaticMarkup(<UsageTotalCard totals={totals} spec={spec("tokensIn")} />);
    expect(markup).toContain("1,800");
    expect(markup).toContain("measured on 10 of 10 rows");
  });
});

describe("scheduler decision rows", () => {
  it("shows the causal chain and the cited reading's own observation time", () => {
    const markup = renderToStaticMarkup(<DecisionRow decision={decision} />);

    expect(markup).toContain("job-chosen");
    expect(markup).toContain("Admitted");
    expect(markup).toContain("chosen over job-second, job-third");
    // Aging promotion is named, since the effective class is what ranked it.
    expect(markup).toContain("class batch, aged up to interactive");
    expect(markup).toContain("available_memory_bytes");
    expect(markup).toContain("9,878,000,000");
    // The measurement's timestamp is labelled as the reading's own, and
    // explicitly distinguished from when the decision was written.
    expect(markup).toContain("Reading observed at");
    expect(markup).toContain("not when this decision was written");
  });

  it("says when the cited reading has no recorded observation time", () => {
    // Never back-filled with the decision time — that would manufacture exactly
    // the citation this column exists to prove.
    const markup = renderToStaticMarkup(
      <DecisionRow decision={{ ...decision, measuredAtMs: null, measuredValue: null }} />,
    );
    expect(markup).toContain("The reading&#x27;s observation time was not recorded");
    expect(markup).toContain("no value recorded");
  });
});
