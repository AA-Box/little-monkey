import { describe, expect, it } from "vitest";

import {
  measurementOf,
  renderMeasurement,
  renderTotal,
  totalIsPartial,
  USAGE_FIELDS,
  type ProcessUsageRow,
  type ProcessUsageTotal,
} from "./processUsage";

const UNEXPLAINED = "unavailable, and the ledger recorded no reason";

function row(overrides: Partial<ProcessUsageRow> = {}): ProcessUsageRow {
  return {
    processId: "proc-1",
    kind: "chat_turn",
    externalId: "turn-1",
    runId: null,
    workspace: null,
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
        { field: "bytesEgressed", reason: "no egress was attributed to this process" },
        { field: "gpuResidentBytes", reason: "no runtime in this build reports GPU residency" },
        { field: "gpuDeviceMs", reason: "no runtime in this build reports GPU device time" },
      ],
    },
    ...overrides,
  };
}

function total(overrides: Partial<ProcessUsageTotal> = {}): ProcessUsageTotal {
  return { value: 100, measuredRows: 3, unavailableRows: 0, ...overrides };
}

describe("resource ledger measurements", () => {
  it("covers every wire field the ledger reports", () => {
    // Guards the one failure mode a rename would cause silently: a field spec
    // whose name no longer matches `process_usage.rs`'s `FIELD_*` consts reads
    // as unavailable forever, because no note will ever match it either.
    expect(USAGE_FIELDS.map((spec) => spec.field)).toEqual([
      "wallTimeMs",
      "cpuTimeMs",
      "peakRssBytes",
      "bytesRead",
      "bytesWritten",
      "bytesEgressed",
      "tokensIn",
      "tokensOut",
      "gpuResidentBytes",
      "gpuDeviceMs",
    ]);
  });

  it("reads wall time from the row and its gap reason from the usage notes", () => {
    // `wallTimeMs` is derived and lives beside `usage`, but its reason lives
    // inside `usage.unavailable` like every other gap.
    const live = row({
      state: "running",
      exitStatus: null,
      wallTimeMs: null,
      usage: {
        ...row().usage,
        unavailable: [
          ...row().usage.unavailable,
          { field: "wallTimeMs", reason: "this process has not exited, so its wall time is not final" },
        ],
      },
    });
    expect(measurementOf(live, "wallTimeMs")).toEqual({
      field: "wallTimeMs",
      value: null,
      reason: "this process has not exited, so its wall time is not final",
    });
    expect(measurementOf(row(), "wallTimeMs")).toEqual({ field: "wallTimeMs", value: 4_000, reason: null });
  });

  it("renders a null measurement as unavailable with its reason, never as 0", () => {
    const rendered = renderMeasurement(measurementOf(row(), "bytesEgressed"), "bytes", UNEXPLAINED);
    expect(rendered.available).toBe(false);
    expect(rendered).toEqual({
      available: false,
      reason: "no egress was attributed to this process",
    });
    // The whole point: nothing in the rendered output can be read as a number.
    expect(JSON.stringify(rendered)).not.toMatch(/\b0\b/);
    expect(JSON.stringify(rendered)).not.toContain("0 B");
  });

  it("still refuses to invent a zero when the reason is missing", () => {
    // Rust makes this unconstructible, but a contract the UI depends on is
    // worth failing safe on: unexplained still means unavailable.
    const orphan = row({
      usage: { ...row().usage, bytesRead: null },
    });
    expect(renderMeasurement(measurementOf(orphan, "bytesRead"), "bytes", UNEXPLAINED)).toEqual({
      available: false,
      reason: UNEXPLAINED,
    });
  });

  it("renders a real zero measurement as zero", () => {
    // The inverse of the rule, and the reason the rule matters: a measured 0 is
    // a fact and must not be hidden behind "unavailable".
    const measured = row({
      usage: {
        ...row().usage,
        bytesEgressed: 0,
        unavailable: row().usage.unavailable.filter((note) => note.field !== "bytesEgressed"),
      },
    });
    expect(renderMeasurement(measurementOf(measured, "bytesEgressed"), "bytes", UNEXPLAINED)).toEqual({
      available: true,
      text: "0 B",
    });
  });

  it("reports an unmeasured total as unknown rather than zero", () => {
    expect(renderTotal(total({ value: null, measuredRows: 0, unavailableRows: 10 }), "bytes", "no total")).toEqual({
      available: false,
      reason: "no total",
    });
  });

  it("flags a total that could not read every row", () => {
    expect(totalIsPartial(total({ measuredRows: 3, unavailableRows: 7 }))).toBe(true);
    expect(totalIsPartial(total({ measuredRows: 10, unavailableRows: 0 }))).toBe(false);
  });
});
